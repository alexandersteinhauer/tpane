//! Direct, bounded PTY transport; no terminal-emulator state enters these tasks.
//!
//! `connect` requires an IO-enabled Tokio runtime. Its duplicate is CLOEXEC;
//! O_NONBLOCK is shared with the caller's open file description. Do not mix
//! this driver with other readers or blocking writers on that description.

use std::{
    collections::VecDeque,
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result, ensure};
use tokio::{
    io::unix::AsyncFd,
    sync::{Notify, Semaphore, mpsc},
    task::JoinHandle,
};

use crate::{
    bus::{Bus, Control, PtyEvent},
    tree::Id,
};

const MAX_PENDING_INPUT: usize = 8 * 1024 * 1024;
// Event size sets the per-event fixed costs (readiness re-arm, bus send,
// credit cycle, allocation) relative to bytes moved. 64 KiB events measured
// faster 24-pane flood output but slower interactive CPU because macOS
// allocates above 32 KiB from its large-allocation zone; 32 KiB stays in the
// small zone. Bounded by readiness exhaustion per event.
pub const CHUNK: usize = 32 * 1024;
const OUTPUT_CREDITS: usize = 16;
// Bounds worst-case work per published event when a descriptor returns tiny
// reads. At the PTY's usual ~1 KiB reads, CHUNK fills first.
const MAX_READS_PER_EVENT: usize = 64;
type Master = Arc<AsyncFd<OwnedFd>>;

pub struct Connection {
    pub writer: Writer,
    pub tasks: Tasks,
}

/// Abort is cooperative: descriptors close when Tokio next drops the aborted
/// futures. Neither the retained Writer nor queued output events own the FD.
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

fn duplicate_master(fd: RawFd) -> Result<Master> {
    // SAFETY: borrow the caller's descriptor, then immediately own the duplicate.
    let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
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
    Ok(Arc::new(
        AsyncFd::new(owned).context("register PTY master")?,
    ))
}

pub fn connect(fd: RawFd, id: Id, bus: Bus) -> Result<Connection> {
    tokio::runtime::Handle::try_current().context("PTY driver requires a Tokio runtime")?;
    let master = duplicate_master(fd)?;
    let input = Arc::new(InputQueue::default());
    let reader_master = Arc::clone(&master);
    let reader = tokio::spawn(async move {
        // Receiver shutdown also cancels an idle read or a wait for pane credit.
        tokio::select! {
            _ = bus.pty.closed() => {},
            _ = read_loop(reader_master, id, &bus.pty) => {},
        }
    });
    // Construct outside the future: abort-before-first-poll must close the queue.
    let guard = WorkerGuard(Arc::clone(&input));
    let writer_task = tokio::spawn(write_loop(master, id, bus.control, guard));
    Ok(Connection {
        writer: Writer { input },
        tasks: Tasks {
            reader,
            writer: writer_task,
        },
    })
}

// One syscall per attempt. EINTR is retried cooperatively outside the guard.
#[cfg(test)]
fn read_fd(fd: RawFd, bytes: &mut [u8]) -> io::Result<usize> {
    // SAFETY: the slice is writable for its full length.
    unsafe { read_fd_uninit(fd, bytes.as_mut_ptr(), bytes.len()) }
}

/// Reads into a caller-allocated region that may be uninitialized.
///
/// # Safety
///
/// `ptr` must be valid for writes of `len` bytes, and the caller must only
/// treat the first returned count as initialized afterwards.
unsafe fn read_fd_uninit(fd: RawFd, ptr: *mut u8, len: usize) -> io::Result<usize> {
    let count = unsafe { libc::read(fd, ptr.cast(), len) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

fn write_fd(fd: RawFd, bytes: &[u8]) -> io::Result<usize> {
    // SAFETY: fd is borrowed and the slice is readable for its full length.
    let count = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
    if count < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(count as usize)
    }
}

enum Terminal {
    Eof,
    Error(io::Error),
}

async fn read_loop(master: Master, id: Id, tx: &mpsc::Sender<PtyEvent>) {
    let credits = Arc::new(Semaphore::new(OUTPUT_CREDITS));
    loop {
        // Credit includes events waiting for the shared bus and events retained
        // by main after recv. Never reserve shared bus slots for idle panes.
        let credit = Arc::clone(&credits).acquire_owned().await.unwrap();
        // Capacity without zeroing: each read initializes its own prefix, and
        // only that prefix becomes the event's length.
        let mut bytes: Vec<u8> = Vec::with_capacity(CHUNK);
        // Never exceed the engine's per-feed budget even if the allocator
        // rounds capacity up.
        let capacity = CHUNK;
        let mut len = 0;
        let mut terminal = None;
        // PTYs commonly deliver far less than CHUNK per read; one bus event,
        // channel send, credit cycle, and wake per read dominates multi-pane
        // workloads. Coalesce reads until the buffer is full, readiness is
        // exhausted, or the attempt budget is hit. Do not yield between reads:
        // measured 24-tab output time rose from 3.1s to 4.4s when a yield was
        // added per successful read, because the worker fell back into the
        // reactor far more often; the per-event yield below is the fairness
        // budget. Readiness is not cleared on success, so intermediate reads do
        // not re-enter the reactor.
        for _ in 0..MAX_READS_PER_EVENT {
            let attempt = match master.readable().await {
                Ok(mut ready) => ready.try_io(|fd| {
                    // SAFETY: writes stay within the uninitialized spare capacity;
                    // len only counts bytes the kernel reports as written.
                    unsafe {
                        read_fd_uninit(
                            fd.get_ref().as_raw_fd(),
                            bytes.as_mut_ptr().add(len),
                            capacity - len,
                        )
                    }
                }),
                Err(error) => {
                    terminal = Some(Terminal::Error(error));
                    break;
                }
            };
            match attempt {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(Ok(0)) => {
                    terminal = Some(Terminal::Eof);
                    break;
                }
                Ok(Ok(count)) => {
                    len += count;
                    if len == capacity {
                        break;
                    }
                }
                // Linux PTY slave hangup is EIO, not a transport failure.
                Ok(Err(error)) if error.raw_os_error() == Some(libc::EIO) => {
                    terminal = Some(Terminal::Eof);
                    break;
                }
                Ok(Err(error)) => {
                    terminal = Some(Terminal::Error(error));
                    break;
                }
                Err(_would_block) => break,
            }
        }
        // SAFETY: the first `len` bytes were initialized by successful reads.
        unsafe { bytes.set_len(len) };
        // Publish accumulated bytes first so terminal events keep FIFO order.
        if bytes.is_empty() {
            drop(credit);
        } else if tx
            .send(PtyEvent::Output {
                id,
                bytes,
                _credit: credit,
            })
            .await
            .is_err()
        {
            return;
        }
        // Terminal events use the SAME FIFO as data, never the control channel.
        match terminal {
            None => {}
            Some(Terminal::Eof) => {
                let _ = tx.send(PtyEvent::Eof(id)).await;
                return;
            }
            Some(Terminal::Error(error)) => {
                let _ = tx.send(PtyEvent::ReadError { id, error }).await;
                return;
            }
        }
        // AsyncFd can stay ready forever. Bound work even on a hot descriptor;
        // this also yields when a readiness wakeup produced no bytes.
        tokio::task::yield_now().await;
    }
}

#[derive(Default)]
struct InputState {
    pending: VecDeque<Vec<u8>>,
    // Counts complete chunks until written, including the task's in-flight chunk.
    outstanding: usize,
    producer_closed: bool,
    stopped: bool,
    error: Option<io::Error>,
}

#[derive(Default)]
struct InputQueue {
    state: Mutex<InputState>,
    ready: Notify,
}

impl InputState {
    fn check(&self) -> Result<()> {
        if let Some(error) = &self.error {
            let copy = match error.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(error.kind(), error.to_string()),
            };
            return Err(copy).context("write terminal input");
        }
        ensure!(!self.stopped, "terminal input worker stopped");
        Ok(())
    }
}

/// Synchronous bounded submission; the task progresses without flush scans.
pub struct Writer {
    input: Arc<InputQueue>,
}

impl Writer {
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut state = self.input.state.lock().unwrap();
        state.check()?;
        ensure!(
            bytes.len() <= MAX_PENDING_INPUT - state.outstanding,
            "terminal input exceeds the 8 MiB queue limit"
        );
        if bytes.is_empty() {
            return Ok(());
        }
        state.outstanding += bytes.len();
        let mut remaining = bytes;
        while !remaining.is_empty() {
            if state.pending.back().is_none_or(|last| last.len() == CHUNK) {
                state.pending.push_back(Vec::with_capacity(CHUNK));
            }
            let last = state.pending.back_mut().unwrap();
            let count = remaining.len().min(CHUNK - last.len());
            last.extend_from_slice(&remaining[..count]);
            remaining = &remaining[count..];
        }
        // All queued chunks except the tail are full, so tiny writes cannot
        // create millions of allocations. At most 129 chunks including in-flight.
        drop(state);
        self.input.ready.notify_one();
        Ok(())
    }

    /// Compatibility/status check only, NOT a kernel-drain guarantee. Never
    /// needed for progress; asynchronous failures also arrive on bus.control.
    #[cfg(test)]
    pub fn flush(&mut self) -> Result<()> {
        self.input.state.lock().unwrap().check()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.input.state.lock().unwrap().producer_closed = true;
        self.input.ready.notify_one();
    }
}

struct WorkerGuard(Arc<InputQueue>);

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap();
        state.stopped = true;
        let queued: usize = state.pending.drain(..).map(|bytes| bytes.len()).sum();
        state.outstanding -= queued;
    }
}

struct InFlight {
    bytes: Vec<u8>,
    input: Arc<InputQueue>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.input.state.lock().unwrap().outstanding -= self.bytes.len();
    }
}

async fn write_all(master: &AsyncFd<OwnedFd>, bytes: &[u8]) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let attempt = {
            let mut ready = master.writable().await?;
            ready.try_io(|fd| {
                write_fd(
                    fd.get_ref().as_raw_fd(),
                    &bytes[offset..bytes.len().min(offset + CHUNK)],
                )
            })
        };
        match attempt {
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(count)) => offset += count,
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => return Err(error),
            Err(_would_block) => {}
        }
        // Includes successful writes, EINTR and spurious readiness.
        tokio::task::yield_now().await;
    }
    Ok(())
}

async fn write_loop(
    master: Master,
    id: Id,
    control: mpsc::UnboundedSender<Control>,
    guard: WorkerGuard,
) {
    loop {
        // Notify stores a permit even if submission races this empty check.
        // There is exactly one consumer, and no lock crosses an await.
        let next = {
            let mut state = guard.0.state.lock().unwrap();
            match state.pending.pop_front() {
                Some(bytes) => Some(InFlight {
                    bytes,
                    input: Arc::clone(&guard.0),
                }),
                None if state.producer_closed => return,
                None => None,
            }
        };
        let Some(input) = next else {
            guard.0.ready.notified().await;
            continue;
        };
        let result = write_all(&master, &input.bytes).await;
        drop(input);
        if let Err(error) = result {
            // Persist a copy for synchronous status checks, publish exactly once.
            guard.0.state.lock().unwrap().error = Some(match error.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(error.kind(), error.to_string()),
            });
            let _ = control.send(Control::WriteError { id, error });
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus;
    use std::{
        net::{TcpListener, TcpStream},
        os::unix::net::UnixStream,
        time::Duration,
    };
    use tokio::time::timeout;

    const TEST_TIMEOUT: Duration = Duration::from_secs(30);

    fn pair(id: Id, bus: Bus) -> (Connection, AsyncFd<OwnedFd>) {
        let (master, peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        // Force writes much smaller than a transport chunk (and WouldBlock).
        let size: libc::c_int = 1024;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    master.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let connection = connect(master.as_raw_fd(), id, bus).unwrap();
        assert_ne!(
            unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) } & libc::O_NONBLOCK,
            0
        );
        drop(master); // Only transport tasks retain this end of the socket.
        (connection, AsyncFd::new(peer.into()).unwrap())
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + i / 251) % 256) as u8).collect()
    }

    async fn peer_read(peer: &AsyncFd<OwnedFd>, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            let attempt = {
                let mut ready = peer.readable().await?;
                ready.try_io(|fd| read_fd(fd.get_ref().as_raw_fd(), bytes))
            };
            match attempt {
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                Ok(result) => return result,
                Err(_) => {}
            }
            tokio::task::yield_now().await;
        }
    }

    async fn receive_bytes(peer: &AsyncFd<OwnedFd>, count: usize) -> Vec<u8> {
        let mut bytes = vec![0; count];
        let mut offset = 0;
        while offset < count {
            let n = peer_read(peer, &mut bytes[offset..]).await.unwrap();
            assert_ne!(n, 0);
            offset += n;
            tokio::task::yield_now().await;
        }
        bytes
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_routing_held_pane_credits_and_eof_after_exact_output() {
        timeout(TEST_TIMEOUT, async {
            let (bus, mut events, mut controls) = bus::channel();
            let (_connection, peer) = pair(11, bus.clone());
            let (_other, other_peer) = pair(22, bus);
            let expected = payload(2 * 1024 * 1024);
            let send = expected.clone();
            let producer = tokio::spawn(async move {
                write_all(&peer, &send).await.unwrap();
            });
            while events.len() < OUTPUT_CREDITS {
                tokio::task::yield_now().await;
            }
            assert_eq!(events.len(), OUTPUT_CREDITS);
            assert!(!producer.is_finished());
            let mut held = Vec::new();
            let mut received = Vec::new();
            for _ in 0..OUTPUT_CREDITS {
                match events.recv().await.unwrap() {
                    PtyEvent::Output { id, bytes, _credit } => {
                        assert_eq!(id, 11);
                        assert!(!bytes.is_empty() && bytes.len() <= CHUNK);
                        received.extend_from_slice(&bytes);
                        held.push(_credit);
                    }
                    _ => panic!("data expected"),
                }
            }
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            assert!(events.is_empty(), "recv must not release pane credits");
            assert!(!producer.is_finished());
            // A pane stalled on its own credits cannot monopolize shared bus.
            write_all(&other_peer, b"other pane").await.unwrap();
            match events.recv().await.unwrap() {
                PtyEvent::Output { id, bytes, .. } => {
                    assert_eq!(id, 22);
                    assert_eq!(bytes, b"other pane");
                }
                _ => panic!("other pane data expected"),
            }
            drop(held.pop());
            match events.recv().await.unwrap() {
                PtyEvent::Output { id, bytes, _credit } => {
                    assert_eq!(id, 11);
                    received.extend_from_slice(&bytes);
                    held.push(_credit);
                }
                _ => panic!("one released credit must allow another chunk"),
            }
            for _ in 0..32 {
                tokio::task::yield_now().await;
            }
            assert!(events.is_empty());
            drop(held);
            loop {
                match events.recv().await.unwrap() {
                    PtyEvent::Output { id, bytes, .. } => {
                        assert_eq!(id, 11);
                        assert!(!bytes.is_empty() && bytes.len() <= CHUNK);
                        received.extend_from_slice(&bytes);
                    }
                    PtyEvent::Eof(id) => {
                        assert_eq!(id, 11);
                        break;
                    }
                    _ => panic!("unexpected read error"),
                }
            }
            producer.await.unwrap();
            assert_eq!(received, expected);
            assert!(events.is_empty());
            assert!(controls.try_recv().is_err(), "EOF must not use control");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn large_partial_input_exact_bounded_including_inflight_without_flush() {
        timeout(TEST_TIMEOUT, async {
            let (bus, _events, _controls) = bus::channel();
            let (mut connection, peer) = pair(1, bus);
            let expected = payload(MAX_PENDING_INPUT);
            connection.writer.write(&expected).unwrap();
            assert!(connection.writer.write(b"overflow").is_err());
            assert_eq!(
                connection.writer.input.state.lock().unwrap().pending.len(),
                MAX_PENDING_INPUT / CHUNK
            );
            // With no peer draining, the first 16KiB chunk cannot finish.
            for _ in 0..64 {
                tokio::task::yield_now().await;
            }
            {
                let state = connection.writer.input.state.lock().unwrap();
                assert_eq!(state.outstanding, MAX_PENDING_INPUT);
                assert_eq!(state.pending.len(), MAX_PENDING_INPUT / CHUNK - 1);
            }
            // Positive kernel data with the entire first chunk still accounted
            // proves a partial write, not merely an unpolled writer task.
            let mut first = 0u8;
            assert_eq!(
                unsafe {
                    libc::recv(
                        peer.as_raw_fd(),
                        (&mut first as *mut u8).cast(),
                        1,
                        libc::MSG_PEEK | libc::MSG_DONTWAIT,
                    )
                },
                1
            );
            assert_eq!(first, expected[0]);
            assert!(connection.writer.write(b"in-flight still counts").is_err());
            assert_eq!(receive_bytes(&peer, expected.len()).await, expected);
            while connection.writer.input.state.lock().unwrap().outstanding != 0 {
                tokio::task::yield_now().await;
            }
            // Progress and capacity restoration require no flush calls/events.
            connection.writer.write(b"restored").unwrap();
            assert_eq!(receive_bytes(&peer, 8).await, b"restored");
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn millions_of_tiny_writes_coalesce_into_bounded_metadata_and_preserve_fifo() {
        timeout(TEST_TIMEOUT, async {
            let (bus, _events, _controls) = bus::channel();
            let (mut connection, peer) = pair(1, bus);
            let expected = payload(MAX_PENDING_INPUT);
            for byte in &expected {
                connection.writer.write(std::slice::from_ref(byte)).unwrap();
            }
            {
                let state = connection.writer.input.state.lock().unwrap();
                assert_eq!(state.outstanding, MAX_PENDING_INPUT);
                assert_eq!(state.pending.len(), MAX_PENDING_INPUT / CHUNK);
                assert!(
                    state
                        .pending
                        .iter()
                        .all(|chunk| chunk.len() == CHUNK && chunk.capacity() == CHUNK)
                );
            }
            assert!(connection.writer.write(b"x").is_err());
            assert_eq!(receive_bytes(&peer, expected.len()).await, expected);
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn write_failure_once_on_control_clears_queue_and_reports_status() {
        timeout(TEST_TIMEOUT, async {
            let (bus, mut events, mut controls) = bus::channel();
            let (mut connection, peer) = pair(41, bus);
            drop(peer);
            connection
                .writer
                .write(&vec![1; MAX_PENDING_INPUT])
                .unwrap();
            match controls.recv().await.unwrap() {
                Control::WriteError { id, error } => {
                    assert_eq!(id, 41);
                    assert!(matches!(
                        error.kind(),
                        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                    ));
                }
                _ => panic!("write error expected"),
            }
            while !connection.tasks.writer.is_finished() {
                tokio::task::yield_now().await;
            }
            assert!(controls.try_recv().is_err());
            assert!(
                connection
                    .writer
                    .flush()
                    .unwrap_err()
                    .downcast_ref::<io::Error>()
                    .is_some()
            );
            assert!(connection.writer.write(b"stopped").is_err());
            assert_eq!(connection.writer.input.state.lock().unwrap().outstanding, 0);
            assert!(
                connection
                    .writer
                    .input
                    .state
                    .lock()
                    .unwrap()
                    .pending
                    .is_empty()
            );
            assert!(matches!(events.recv().await.unwrap(), PtyEvent::Eof(41)));
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_before_first_poll_and_while_blocked_releases_fd_and_counts() {
        timeout(TEST_TIMEOUT, async {
            for poll_first in [false, true] {
                let (bus, _events, mut controls) = bus::channel();
                let (mut connection, peer) = pair(1, bus);
                connection
                    .writer
                    .write(&vec![0; MAX_PENDING_INPUT])
                    .unwrap();
                if poll_first {
                    for _ in 0..64 {
                        tokio::task::yield_now().await;
                    }
                }
                let Connection { mut writer, tasks } = connection;
                drop(tasks);
                // Retain Writer and bus receiver: neither may keep FD/input alive.
                let mut buffer = [0; CHUNK];
                while peer_read(&peer, &mut buffer).await.unwrap() != 0 {}
                assert_eq!(writer.input.state.lock().unwrap().outstanding, 0);
                assert!(writer.input.state.lock().unwrap().pending.is_empty());
                assert!(writer.write(b"cancelled").is_err());
                assert!(
                    controls.try_recv().is_err(),
                    "cancellation is not a write failure"
                );
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn dropping_writer_drains_fifo_and_dropping_bus_stops_idle_reader() {
        timeout(TEST_TIMEOUT, async {
            let (bus, events, _controls) = bus::channel();
            let (mut connection, peer) = pair(1, bus);
            let expected = payload(128 * 1024);
            connection.writer.write(&expected).unwrap();
            let Connection { writer, tasks } = connection;
            drop(writer);
            drop(events);
            assert_eq!(receive_bytes(&peer, expected.len()).await, expected);
            assert_eq!(peer_read(&peer, &mut [0; 1]).await.unwrap(), 0);
            assert!(tasks.reader.is_finished());
            assert!(tasks.writer.is_finished());
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_error_is_on_data_bus_after_preceding_output() {
        timeout(TEST_TIMEOUT, async {
            // TCP reset gives a genuine non-EIO read error without invalidating
            // an owned descriptor or relying on a mocked readiness source.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (master, _) = listener.accept().unwrap();
            peer.set_nonblocking(true).unwrap();
            let peer = AsyncFd::new(OwnedFd::from(peer)).unwrap();
            let (bus, mut events, mut controls) = bus::channel();
            let _connection = connect(master.as_raw_fd(), 73, bus).unwrap();
            drop(master);
            write_all(&peer, b"before reset").await.unwrap();
            while events.is_empty() {
                tokio::task::yield_now().await;
            }
            let linger = libc::linger {
                l_onoff: 1,
                l_linger: 0,
            };
            assert_eq!(
                unsafe {
                    libc::setsockopt(
                        peer.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_LINGER,
                        (&linger as *const libc::linger).cast(),
                        std::mem::size_of_val(&linger) as libc::socklen_t,
                    )
                },
                0
            );
            drop(peer);
            let mut received = Vec::new();
            loop {
                match events.recv().await.unwrap() {
                    PtyEvent::Output { id, bytes, .. } => {
                        assert_eq!(id, 73);
                        received.extend_from_slice(&bytes);
                    }
                    PtyEvent::ReadError { id, error } => {
                        assert_eq!(id, 73);
                        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
                        break;
                    }
                    _ => panic!("expected reset, not EOF"),
                }
            }
            assert_eq!(received, b"before reset");
            assert!(controls.try_recv().is_err());
        })
        .await
        .unwrap();
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test(flavor = "current_thread")]
    async fn real_pty_hangup_is_ordered_eof_and_duplicate_is_cloexec() {
        timeout(TEST_TIMEOUT, async {
            let mut master = -1;
            let mut slave = -1;
            assert_eq!(
                unsafe {
                    libc::openpty(
                        &mut master,
                        &mut slave,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                    )
                },
                0
            );
            let master = unsafe { OwnedFd::from_raw_fd(master) };
            let slave = unsafe { OwnedFd::from_raw_fd(slave) };
            let (bus, mut events, _controls) = bus::channel();
            let _connection = connect(master.as_raw_fd(), 19, bus).unwrap();
            // Exercise the exact duplication/registration path used by connect.
            let duplicate = duplicate_master(master.as_raw_fd()).unwrap();
            assert_ne!(
                unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
            assert_eq!(write_fd(slave.as_raw_fd(), b"pty output").unwrap(), 10);
            // Let transport observe data before PTY hangup (some kernels discard
            // unread PTY data on last slave close).
            while events.is_empty() {
                tokio::task::yield_now().await;
            }
            drop(slave);
            match events.recv().await.unwrap() {
                PtyEvent::Output { id, bytes, .. } => {
                    assert_eq!(id, 19);
                    assert_eq!(bytes, b"pty output");
                }
                _ => panic!("PTY output expected first"),
            }
            assert!(matches!(events.recv().await.unwrap(), PtyEvent::Eof(19)));
        })
        .await
        .unwrap();
    }
}
