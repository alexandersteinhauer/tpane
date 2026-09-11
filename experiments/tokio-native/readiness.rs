//! Readiness-only PTY transport. The coordinator owns the read budget and buffer.
//!
//! A pane has one non-cloneable token, including while its event waits for bus
//! capacity. Keep that token across bounded coordinator turns; drop it only
//! after WouldBlock, or stop it before dropping on EOF/error/pane closure.
//! The caller must cancel `watch` on channel closure and when removing a pane.

use std::{
    io,
    os::fd::{AsRawFd, OwnedFd},
    sync::Arc,
};

use tokio::{
    io::{Interest, unix::AsyncFd},
    sync::{Notify, mpsc},
};

use crate::{bus::PtyEvent, tree::Id};

/// The descriptor must be nonblocking and have no competing reader.
pub type Master = Arc<AsyncFd<OwnedFd>>;

/// Exclusive permission to service one pane's readable descriptor.
///
/// Dropping acknowledges the notification, allowing the watcher to rearm.
/// This deliberately does not implement Clone.
pub struct ReadReady {
    master: Master,
    rearm: Arc<Notify>,
    stopped: bool,
}

impl ReadReady {
    /// Make at most one read syscall directly into the coordinator's buffer.
    /// Only WouldBlock clears Tokio readiness; EINTR, EIO and EOF are left for
    /// the coordinator to interpret. An empty buffer must not look like EOF.
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "readiness reads require a nonempty buffer",
            ));
        }
        self.master.try_io(Interest::READABLE, |fd| {
            // SAFETY: the token owns the descriptor and buffer is writable for
            // its entire length. The descriptor is nonblocking by contract.
            let count =
                unsafe { libc::read(fd.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(count as usize)
            }
        })
    }

    /// Suppress the drop acknowledgement permanently. The owner must also
    /// cancel the watcher; it remains parked rather than emitting another token.
    pub fn stop(&mut self) {
        self.stopped = true;
    }
}

impl Drop for ReadReady {
    fn drop(&mut self) {
        if !self.stopped {
            self.rearm.notify_one();
        }
    }
}

/// Publish readiness, never bytes. The transport wraps this in a select against
/// `tx.closed()` so idle, stopped and acknowledgement waits are cancellable.
pub async fn watch(master: Master, id: Id, tx: &mpsc::Sender<PtyEvent>) {
    let rearm = Arc::new(Notify::new());
    loop {
        match master.readable().await {
            // Dropping the guard MUST NOT clear readiness: the coordinator's
            // try_io read clears it only when the actual syscall WouldBlocks.
            Ok(guard) => drop(guard),
            Err(error) => {
                let _ = tx.send(PtyEvent::ReadError { id, error }).await;
                return;
            }
        }
        let ready = ReadReady {
            master: Arc::clone(&master),
            rearm: Arc::clone(&rearm),
            stopped: false,
        };
        if tx.send(PtyEvent::Ready { id, ready }).await.is_err() {
            return;
        }
        // notify_one stores a permit even if the coordinator drops the token
        // before this future is polled. There is exactly one token/ack per lap.
        rearm.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        sync::Weak,
        time::Duration,
    };
    use tokio::{task::JoinHandle, time::timeout};

    const DEADLINE: Duration = Duration::from_secs(3);
    const QUIET: Duration = Duration::from_millis(25);

    fn pair() -> (Master, UnixStream) {
        let (reader, peer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        (Arc::new(AsyncFd::new(reader.into()).unwrap()), peer)
    }

    // Match the transport's lifetime contract, including channel cancellation.
    fn spawn(master: Master, id: Id, tx: mpsc::Sender<PtyEvent>) -> JoinHandle<()> {
        tokio::spawn(async move {
            tokio::select! {
                _ = tx.closed() => {},
                _ = watch(master, id, &tx) => {},
            }
        })
    }

    async fn next(rx: &mut mpsc::Receiver<PtyEvent>) -> (Id, ReadReady) {
        match timeout(DEADLINE, rx.recv()).await.expect("watcher stalled") {
            Some(PtyEvent::Ready { id, ready }) => (id, ready),
            Some(PtyEvent::ReadError { error, .. }) => panic!("readiness error: {error}"),
            None => panic!("watcher unexpectedly closed the channel"),
        }
    }

    async fn quiet(rx: &mut mpsc::Receiver<PtyEvent>) {
        assert!(
            timeout(QUIET, rx.recv()).await.is_err(),
            "unexpected event or closure"
        );
    }

    async fn abort(task: JoinHandle<()>) {
        task.abort();
        assert!(
            timeout(DEADLINE, task)
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
    }

    fn released(weak: &Weak<AsyncFd<OwnedFd>>, peer: &mut UnixStream) {
        assert!(weak.upgrade().is_none(), "descriptor still owned");
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap(),
            0,
            "peer must observe closure"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_token_while_held_even_with_more_data() {
        let (master, mut peer) = pair();
        let (tx, mut rx) = mpsc::channel(8);
        let task = spawn(master, 11, tx);
        peer.write_all(b"first").unwrap();
        let (id, mut ready) = next(&mut rx).await;
        assert_eq!(id, 11);
        peer.write_all(b"second").unwrap();
        quiet(&mut rx).await;
        let mut bytes = [0; 11];
        assert_eq!(ready.read(&mut bytes).unwrap(), 11);
        assert_eq!(&bytes, b"firstsecond");
        quiet(&mut rx).await;
        ready.stop();
        drop(ready);
        abort(task).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn direct_reads_preserve_exact_bytes_before_eof() {
        let (master, mut peer) = pair();
        let (tx, mut rx) = mpsc::channel(8);
        let task = spawn(master, 12, tx);
        let expected: Vec<u8> = (0..1024).map(|n| (n % 251) as u8).collect();
        peer.write_all(&expected).unwrap();
        peer.shutdown(std::net::Shutdown::Write).unwrap();
        let (_, mut ready) = next(&mut rx).await;
        assert_eq!(
            ready.read(&mut []).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        let mut actual = Vec::new();
        let mut buffer = [0; 17];
        loop {
            let count = ready.read(&mut buffer).unwrap();
            if count == 0 {
                break;
            }
            actual.extend_from_slice(&buffer[..count]);
            // Retain the same token across bounded coordinator turns.
            tokio::task::yield_now().await;
            assert!(matches!(
                rx.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
        assert_eq!(actual, expected);
        ready.stop();
        drop(ready);
        quiet(&mut rx).await;
        abort(task).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn would_block_rearms_with_data_before_or_after_ack() {
        for before_ack in [true, false] {
            let (master, mut peer) = pair();
            let (tx, mut rx) = mpsc::channel(8);
            let task = spawn(master, 13, tx);
            peer.write_all(b"a").unwrap();
            let (_, ready) = next(&mut rx).await;
            let mut buffer = [0; 8];
            assert_eq!(ready.read(&mut buffer).unwrap(), 1);
            assert_eq!(buffer[0], b'a');
            assert_eq!(
                ready.read(&mut buffer).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            if before_ack {
                peer.write_all(b"bc").unwrap();
                // Let the reactor see new data while the watcher still waits
                // for acknowledgement; it must not produce another token.
                quiet(&mut rx).await;
            }
            drop(ready);
            if !before_ack {
                // No stale notification after a syscall cleared readiness.
                quiet(&mut rx).await;
                peer.write_all(b"bc").unwrap();
            }
            let (id, mut ready) = next(&mut rx).await;
            assert_eq!(id, 13);
            assert_eq!(ready.read(&mut buffer).unwrap(), 2);
            assert_eq!(&buffer[..2], b"bc");
            ready.stop();
            drop(ready);
            abort(task).await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_suppresses_rearm_even_when_descriptor_stays_readable() {
        let (master, mut peer) = pair();
        let (tx, mut rx) = mpsc::channel(8);
        let task = spawn(master, 14, tx);
        peer.write_all(b"unread").unwrap();
        let (_, mut ready) = next(&mut rx).await;
        ready.stop();
        ready.stop();
        drop(ready);
        peer.write_all(b"more").unwrap();
        quiet(&mut rx).await;
        abort(task).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn independent_panes_progress_while_another_token_is_held() {
        let (a, mut peer_a) = pair();
        let (b, mut peer_b) = pair();
        let (tx, mut rx) = mpsc::channel(1);
        let task_a = spawn(a, 21, tx.clone());
        let task_b = spawn(b, 22, tx);
        peer_a.write_all(b"held").unwrap();
        let (id, mut held) = next(&mut rx).await;
        assert_eq!(id, 21);
        for byte in b"progress" {
            peer_b.write_all(&[*byte]).unwrap();
            let (id, ready) = next(&mut rx).await;
            assert_eq!(id, 22);
            let mut buffer = [0; 1];
            assert_eq!(ready.read(&mut buffer).unwrap(), 1);
            assert_eq!(buffer[0], *byte);
            assert_eq!(
                ready.read(&mut buffer).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            drop(ready);
        }
        held.stop();
        drop(held);
        abort(task_a).await;
        abort(task_b).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_releases_idle_and_acknowledgement_waiters() {
        for send_data in [false, true] {
            let (master, mut peer) = pair();
            let weak = Arc::downgrade(&master);
            let (tx, mut rx) = mpsc::channel(1);
            let task = spawn(master, 31, tx);
            if send_data {
                peer.write_all(b"x").unwrap();
                let (_, mut ready) = next(&mut rx).await;
                let mut buffer = [0; 1];
                assert_eq!(ready.read(&mut buffer).unwrap(), 1);
                ready.stop();
                drop(ready);
            } else {
                tokio::task::yield_now().await;
            }
            abort(task).await;
            released(&weak, &mut peer);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_closure_cancels_idle_and_held_token_waiters() {
        for send_data in [false, true] {
            let (master, mut peer) = pair();
            let weak = Arc::downgrade(&master);
            let (tx, mut rx) = mpsc::channel(1);
            let task = spawn(master, 32, tx);
            let held = if send_data {
                peer.write_all(b"x").unwrap();
                let (_, ready) = next(&mut rx).await;
                assert_eq!(ready.read(&mut [0; 1]).unwrap(), 1);
                Some(ready)
            } else {
                None
            };
            drop(rx);
            timeout(DEADLINE, task).await.unwrap().unwrap();
            if held.is_some() {
                // An event retained by the coordinator intentionally owns its
                // FD even after the watcher exits. Dropping it releases the FD.
                assert!(weak.upgrade().is_some());
            }
            drop(held);
            released(&weak, &mut peer);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn closure_drops_queued_tokens_and_cancels_backpressured_send() {
        let (a, mut peer_a) = pair();
        let (b, mut peer_b) = pair();
        let weak_a = Arc::downgrade(&a);
        let weak_b = Arc::downgrade(&b);
        let (tx, mut rx) = mpsc::channel(1);
        let task_a = spawn(a, 41, tx.clone());
        peer_a.write_all(b"a").unwrap();
        // Receive and drain A, then rearm it with EOF to leave a token queued.
        let (_, ready) = next(&mut rx).await;
        assert_eq!(ready.read(&mut [0; 1]).unwrap(), 1);
        peer_a.shutdown(std::net::Shutdown::Write).unwrap();
        drop(ready);
        timeout(DEADLINE, async {
            while rx.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        peer_b.shutdown(std::net::Shutdown::Write).unwrap();
        let task_b = spawn(b, 42, tx);
        // B's readable EOF must reach its send, blocked behind A's token.
        timeout(DEADLINE, async {
            while weak_b.strong_count() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(rx);
        timeout(DEADLINE, task_a).await.unwrap().unwrap();
        timeout(DEADLINE, task_b).await.unwrap().unwrap();
        released(&weak_a, &mut peer_a);
        released(&weak_b, &mut peer_b);
    }
}
