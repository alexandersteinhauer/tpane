//! Isolated AsyncFd PTY transport. No terminal-emulator state enters these tasks.
//!
//! `connect` must run inside an IO-enabled Tokio runtime. The duplicated master is
//! CLOEXEC, but O_NONBLOCK is a *shared open-file-description* flag: setting it
//! also changes the caller's master and any other duplicates. Do not mix this
//! driver with blocking readers/writers on the same PTY.

use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc as std_mpsc,
    },
    thread,
};

use anyhow::{Context, Result, ensure};
use crossterm::event::{self, Event};
use tokio::{io::unix::AsyncFd, sync::mpsc, task::JoinHandle};

use crate::wake::Wake;

const MAX_PENDING_INPUT: usize = 8 * 1024 * 1024;
const CHANNEL_CAPACITY: usize = 16;
const READ_CHUNK: usize = 16 * 1024;

type Master = Arc<AsyncFd<OwnedFd>>;

pub struct Connection {
    pub output: mpsc::Receiver<io::Result<Vec<u8>>>,
    pub writer: Writer,
    pub tasks: Tasks,
}

/// Owns the task lifetime, independently of the UI's output receiver/writer.
/// Abort is cooperative: the last master reference closes after the runtime
/// next polls the aborted tasks (not necessarily synchronously in `drop`).
pub struct Tasks {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

impl Drop for Tasks {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

pub fn connect(master_fd: RawFd, wake: Wake) -> Result<Connection> {
    tokio::runtime::Handle::try_current().context("PTY driver requires a Tokio runtime")?;
    // SAFETY: fcntl borrows the caller's live descriptor. Ownership of the new
    // descriptor transfers immediately to OwnedFd, including all error paths.
    let duplicate = unsafe { libc::fcntl(master_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(io::Error::last_os_error()).context("duplicate PTY master");
    }
    let owned = unsafe { OwnedFd::from_raw_fd(duplicate) };
    let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(io::Error::last_os_error()).context("set PTY master nonblocking");
    }
    let master = Arc::new(AsyncFd::new(owned).context("register PTY master")?);
    let (output_tx, output) = mpsc::channel(CHANNEL_CAPACITY);
    let (input_tx, input_rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (error_tx, errors) = mpsc::channel(1);
    let outstanding = Arc::new(AtomicUsize::new(0));
    let reader = tokio::spawn(read_loop(Arc::clone(&master), output_tx, wake.clone()));
    let writer_task = tokio::spawn(write_loop(master, input_rx, error_tx, wake));
    Ok(Connection {
        output,
        writer: Writer {
            tx: input_tx,
            pending: VecDeque::new(),
            outstanding,
            errors,
        },
        tasks: Tasks {
            reader,
            writer: writer_task,
        },
    })
}

// These small wrappers do exactly one syscall. EINTR is retried by the async
// loops, with a yield, rather than spinning indefinitely inside a readiness guard.
fn read_fd(fd: RawFd, bytes: &mut [u8]) -> io::Result<usize> {
    // SAFETY: bytes is writable for its entire length; fd is borrowed for this call.
    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

fn write_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: bytes is readable for its entire length; fd is borrowed for this call.
    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

async fn read_loop(master: Master, tx: mpsc::Sender<io::Result<Vec<u8>>>, wake: Wake) {
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        // Reserve before reading: at most 16 queued chunks plus the fixed scratch
        // buffer. Waiting for a slow UI never blocks the runtime thread.
        let Ok(permit) = tx.reserve().await else {
            break;
        };
        let result = loop {
            let attempt = match master.readable().await {
                Ok(mut ready) => ready.try_io(|fd| read_fd(fd.get_ref().as_raw_fd(), &mut buffer)),
                Err(error) => break Err(error),
            };
            match attempt {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(result) => break result,
                Err(_would_block) => {}
            }
            tokio::task::yield_now().await;
        };
        match result {
            Ok(0) => break,
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Ok(count) => {
                permit.send(Ok(buffer[..count].to_vec()));
                wake.notify();
            }
            Err(error) => {
                permit.send(Err(error));
                break;
            }
        }
        // AsyncFd can stay ready forever. Always allow UI work/cancellation to
        // run, even when neither readiness nor channel capacity needs to wait.
        tokio::task::yield_now().await;
    }
    drop(tx);
    wake.notify(); // Includes EOF, error and receiver disconnection.
}

/// Account for a complete write until it finishes, matching the blocking
/// prototype. RAII also releases queued/in-flight bytes on failure or abort.
struct Input {
    bytes: Vec<u8>,
    outstanding: Arc<AtomicUsize>,
}

impl Drop for Input {
    fn drop(&mut self) {
        self.outstanding
            .fetch_sub(self.bytes.len(), Ordering::Relaxed);
    }
}

async fn write_all(master: &AsyncFd<OwnedFd>, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let attempt = {
            let mut ready = master.writable().await?;
            ready.try_io(|fd| write_fd(fd.get_ref().as_raw_fd(), &bytes[offset..]))
        };
        match attempt {
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(count)) => offset += count,
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
        }
        tokio::task::yield_now().await;
    }
    Ok(())
}

async fn write_loop(
    master: Master,
    mut rx: mpsc::Receiver<Input>,
    errors: mpsc::Sender<io::Error>,
    wake: Wake,
) {
    while let Some(input) = rx.recv().await {
        let result = write_all(&master, &input.bytes).await;
        drop(input);
        if let Err(error) = result {
            // Publish the error before closing input, so flush can report it.
            let _ = errors.try_send(error);
            break;
        }
        wake.notify(); // Capacity has become available; the UI can flush pending.
    }
    drop(rx);
    wake.notify();
}

pub struct Writer {
    tx: mpsc::Sender<Input>,
    pending: VecDeque<Input>,
    outstanding: Arc<AtomicUsize>,
    errors: mpsc::Receiver<io::Error>,
}

impl Writer {
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        ensure!(
            bytes.len()
                <= MAX_PENDING_INPUT.saturating_sub(self.outstanding.load(Ordering::Relaxed)),
            "terminal input exceeds the 8 MiB queue limit"
        );
        let bytes = bytes.to_vec();
        self.outstanding.fetch_add(bytes.len(), Ordering::Relaxed);
        self.pending.push_back(Input {
            bytes,
            outstanding: Arc::clone(&self.outstanding),
        });
        self.flush()
    }

    /// Nonblocking submission, not a wait for the kernel to consume all input.
    /// Call again on wakeups, just like the production thread-backed writer.
    pub fn flush(&mut self) -> Result<()> {
        if let Ok(error) = self.errors.try_recv() {
            return Err(error).context("write terminal input");
        }
        while let Some(input) = self.pending.pop_front() {
            match self.tx.try_send(input) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(input)) => {
                    self.pending.push_front(input);
                    break;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    anyhow::bail!("terminal input worker stopped")
                }
            }
        }
        Ok(())
    }
}

/// Terminal UI events still use the one process-wide blocking input thread.
pub fn read_input(wake: Wake) -> Result<std_mpsc::Receiver<io::Result<Event>>> {
    let (tx, rx) = std_mpsc::sync_channel(256);
    thread::Builder::new()
        .name("terminal-input".into())
        .spawn(move || {
            loop {
                let event = event::read();
                let failed = event.is_err();
                if tx.send(event).is_err() {
                    break;
                }
                wake.notify();
                if failed {
                    break;
                }
            }
        })?;
    Ok(rx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::net::UnixStream, time::Duration};
    use tokio::time::timeout;

    const TEST_TIMEOUT: Duration = Duration::from_secs(10);

    fn pair() -> (Connection, AsyncFd<OwnedFd>) {
        let (master, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let connection = connect(master.as_raw_fd(), Wake::new()).unwrap();
        // Only the driver's CLOEXEC duplicate now keeps this end alive.
        drop(master);
        (connection, AsyncFd::new(peer.into()).unwrap())
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + i / 251) % 256) as u8).collect()
    }

    async fn peer_read(peer: &AsyncFd<OwnedFd>, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            let result = {
                let mut ready = peer.readable().await?;
                ready.try_io(|fd| read_fd(fd.get_ref().as_raw_fd(), bytes))
            };
            match result {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(result) => return result,
                Err(_) => {}
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_output_is_exact_chunked_and_backpressured_then_eof() {
        timeout(TEST_TIMEOUT, async {
            let (mut connection, peer) = pair();
            let expected = payload(2 * 1024 * 1024); // Much larger than a socket/PTY buffer.
            let send = expected.clone();
            let producer = tokio::spawn(async move {
                write_all(&peer, &send).await.unwrap();
                // Closing peer produces EOF only after every queued byte.
            });
            while connection.output.len() < CHANNEL_CAPACITY {
                tokio::task::yield_now().await;
            }
            assert!(
                !producer.is_finished(),
                "a full output channel must stop kernel reads"
            );
            assert_eq!(connection.output.len(), CHANNEL_CAPACITY);
            let mut received = Vec::new();
            while let Some(chunk) = connection.output.recv().await {
                let chunk = chunk.unwrap();
                assert!(!chunk.is_empty() && chunk.len() <= READ_CHUNK);
                received.extend_from_slice(&chunk);
            }
            producer.await.unwrap();
            assert_eq!(received, expected);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_partial_input_preserves_order_and_bounds_pending_channel_and_inflight() {
        timeout(TEST_TIMEOUT, async {
            let (mut connection, peer) = pair();
            let expected = payload(MAX_PENDING_INPUT);
            // 32 writes exercise all of: local pending, channel and in-flight.
            for chunk in expected.chunks(MAX_PENDING_INPUT / 32) {
                connection.writer.write(chunk).unwrap();
            }
            assert_eq!(
                connection.writer.outstanding.load(Ordering::Relaxed),
                MAX_PENDING_INPUT
            );
            assert_eq!(connection.writer.pending.len(), 16);
            assert!(connection.writer.write(b"overflow").is_err());
            connection.writer.flush().unwrap(); // Cannot block the sole runtime thread.
            // Let the writer fill the kernel buffer without draining the peer.
            for _ in 0..64 {
                tokio::task::yield_now().await;
                connection.writer.flush().unwrap();
            }
            let count = connection.writer.outstanding.load(Ordering::Relaxed);
            assert!(count > 0, "input must exceed the kernel buffer");
            assert!(
                connection
                    .writer
                    .write(&vec![0; MAX_PENDING_INPUT - count + 1])
                    .is_err()
            );
            let consumer = tokio::spawn(async move {
                let mut received = vec![0; MAX_PENDING_INPUT];
                let mut offset = 0;
                while offset < received.len() {
                    let count = peer_read(&peer, &mut received[offset..]).await.unwrap();
                    assert_ne!(count, 0);
                    offset += count;
                    tokio::task::yield_now().await;
                }
                received
            });
            while connection.writer.outstanding.load(Ordering::Relaxed) != 0 {
                connection.writer.flush().unwrap();
                tokio::task::yield_now().await;
            }
            assert!(connection.writer.pending.is_empty());
            assert_eq!(consumer.await.unwrap(), expected);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_maximum_sized_input_is_written_exactly_across_partial_writes() {
        timeout(TEST_TIMEOUT, async {
            let (mut connection, peer) = pair();
            let expected = payload(MAX_PENDING_INPUT);
            connection.writer.write(&expected).unwrap();
            assert!(connection.writer.write(b"x").is_err());
            let mut received = vec![0; expected.len()];
            let mut offset = 0;
            while offset < received.len() {
                let count = peer_read(&peer, &mut received[offset..]).await.unwrap();
                assert_ne!(count, 0);
                offset += count;
                tokio::task::yield_now().await;
            }
            while connection.writer.outstanding.load(Ordering::Relaxed) != 0 {
                tokio::task::yield_now().await;
            }
            assert_eq!(received, expected);
            connection.writer.write(b"capacity restored").unwrap();
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_eof_and_write_error_are_reported_without_spinning() {
        timeout(TEST_TIMEOUT, async {
            let (mut connection, peer) = pair();
            drop(peer);
            connection.writer.write(b"closed peer").unwrap();
            assert!(connection.output.recv().await.is_none());
            while !connection.tasks.writer.is_finished() {
                tokio::task::yield_now().await;
            }
            let error = connection.writer.flush().unwrap_err();
            assert!(error.to_string().contains("write terminal input"));
            let error = error.downcast_ref::<io::Error>().unwrap();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
            ));
            assert_eq!(connection.writer.outstanding.load(Ordering::Relaxed), 0);
            assert!(connection.writer.write(b"worker stopped").is_err());
            assert_eq!(connection.writer.outstanding.load(Ordering::Relaxed), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_closes_duplicate_even_while_writer_and_output_are_retained() {
        timeout(TEST_TIMEOUT, async {
            let (connection, peer) = pair();
            let Connection {
                mut output,
                mut writer,
                tasks,
            } = connection;
            // Both tasks get a chance to park on readiness/channel receives.
            tokio::task::yield_now().await;
            drop(tasks);
            assert_eq!(peer_read(&peer, &mut [0; 1]).await.unwrap(), 0);
            assert!(output.recv().await.is_none());
            assert!(writer.write(b"cancelled").is_err());
            assert_eq!(writer.outstanding.load(Ordering::Relaxed), 0);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_releases_full_queues_and_inflight_accounting() {
        timeout(TEST_TIMEOUT, async {
            let (mut connection, peer) = pair();
            for _ in 0..32 {
                connection
                    .writer
                    .write(&vec![b'x'; MAX_PENDING_INPUT / 32])
                    .unwrap();
            }
            tokio::task::yield_now().await;
            let count = Arc::clone(&connection.writer.outstanding);
            drop(connection.tasks);
            drop(connection.writer); // Releases the local pending queue, too.
            let mut buffer = [0; READ_CHUNK];
            while peer_read(&peer, &mut buffer).await.unwrap() != 0 {}
            assert!(connection.output.recv().await.is_none());
            assert_eq!(count.load(Ordering::Relaxed), 0);
        })
        .await
        .unwrap();
    }

    // No shell, terminal emulator, or extra PTY dependency needed for hangup.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test(flavor = "current_thread")]
    async fn real_pty_slave_hangup_is_clean_eof_including_eio() {
        timeout(TEST_TIMEOUT, async {
            let mut master = -1;
            let mut slave = -1;
            // SAFETY: openpty initializes both descriptor outputs; null optional
            // arguments request default terminal attributes and window size.
            let result = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(result, 0, "{}", io::Error::last_os_error());
            let master = unsafe { OwnedFd::from_raw_fd(master) };
            let slave = unsafe { OwnedFd::from_raw_fd(slave) };
            let mut connection = connect(master.as_raw_fd(), Wake::new()).unwrap();
            let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(
                flags & libc::O_NONBLOCK,
                0,
                "status flags are shared by dup"
            );
            drop(slave);
            // Linux and Darwin commonly return EIO for this PTY state; accept
            // zero too, since both represent EOF to the driver.
            match read_fd(master.as_raw_fd(), &mut [0; 1]) {
                Ok(0) => {}
                Err(error) if error.raw_os_error() == Some(libc::EIO) => {}
                result => panic!("unexpected PTY hangup result: {result:?}"),
            }
            drop(master);
            assert!(connection.output.recv().await.is_none());
        })
        .await
        .unwrap();
    }
}
