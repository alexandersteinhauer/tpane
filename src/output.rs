//! One ordered, nonblocking host writer. Only the owner submits frames; it must
//! wait for FrameWritten before submitting the next incremental frame.
use std::{
    io::{self, Result},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    time::Duration,
};
use tokio::{
    io::{Interest, unix::AsyncFd},
    sync::mpsc,
    task::JoinHandle,
};

use crate::bus::{Bus, Control};

struct NonblockingFd {
    fd: OwnedFd,
    original_flags: libc::c_int,
}

impl AsRawFd for NonblockingFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl Drop for NonblockingFd {
    fn drop(&mut self) {
        // dup shares file-status flags with the caller's descriptor. Restore
        // them before closing our duplicate, including on cancellation/error.
        loop {
            if unsafe { libc::fcntl(self.as_raw_fd(), libc::F_SETFL, self.original_flags) } >= 0
                || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
            {
                break;
            }
        }
    }
}

enum Message {
    Frame(Vec<u8>),
    Finish(Vec<u8>),
}

pub struct HostOutput {
    sender: Option<mpsc::Sender<Message>>,
    task: Option<JoinHandle<Result<()>>>,
}

impl HostOutput {
    pub fn new(fd: RawFd, bus: Bus) -> Result<Self> {
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return Err(io::Error::last_os_error());
        }
        let duplicate = unsafe { OwnedFd::from_raw_fd(duplicate) };
        let original_flags = unsafe { libc::fcntl(duplicate.as_raw_fd(), libc::F_GETFL) };
        if original_flags < 0 {
            return Err(io::Error::last_os_error());
        }
        let duplicate = NonblockingFd {
            fd: duplicate,
            original_flags,
        };
        if unsafe {
            libc::fcntl(
                duplicate.as_raw_fd(),
                libc::F_SETFL,
                original_flags | libc::O_NONBLOCK,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let output = AsyncFd::with_interest(duplicate, Interest::WRITABLE)?;
        let (sender, mut receiver) = mpsc::channel(1);
        let task = tokio::spawn(async move {
            while let Some(message) = receiver.recv().await {
                match message {
                    Message::Frame(bytes) => {
                        let result = write_all(&output, &bytes).await;
                        // Move the original error into the control event, keeping
                        // raw OS errors intact. Only this task emits completion.
                        let failure = result.as_ref().err().map(copy_error);
                        let delivered = bus.control.send(Control::FrameWritten {
                            result,
                            frame: bytes,
                        });
                        if let Some(error) = failure {
                            return Err(error);
                        }
                        if delivered.is_err() {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "control receiver closed",
                            ));
                        }
                    }
                    Message::Finish(reset) => return write_all(&output, &reset).await,
                }
            }
            Ok(())
        });
        Ok(Self {
            sender: Some(sender),
            task: Some(task),
        })
    }

    /// Never waits for host output. A full queue is a caller protocol error,
    /// not an invitation to discard/replace an older incremental frame.
    pub fn send_frame(&mut self, frame: Vec<u8>) -> Result<()> {
        let sender = self.sender.as_ref().ok_or_else(closed)?;
        sender
            .try_send(Message::Frame(frame))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "frame already queued; wait for FrameWritten",
                ),
                mpsc::error::TrySendError::Closed(_) => closed(),
            })
    }

    /// Append reset after all accepted frame bytes and bound the entire wait.
    /// Success means bytes reached the host fd, not that a terminal displayed
    /// them. Timeout may leave a partial frame/reset; visual restore is unknown.
    /// On return the task has stopped and the shared fd flags are restored.
    pub async fn finish(&mut self, reset: &[u8], timeout: Duration) -> Result<()> {
        let sender = self.sender.take().ok_or_else(closed)?;
        let mut joined = false;
        let result = tokio::time::timeout(timeout, async {
            // If the writer already failed, joining it preserves that error
            // rather than substituting the queue's generic Closed error.
            let _ = sender.send(Message::Finish(reset.to_vec())).await;
            drop(sender);
            let result = self.task.as_mut().expect("writer task exists").await;
            joined = true;
            result.map_err(io::Error::other)?
        })
        .await;
        let task = self.task.take().expect("writer task exists");
        if !joined {
            task.abort();
            // Await cancellation so AsyncFd and its flag guard are gone now,
            // not merely scheduled for destruction on a later runtime turn.
            let _ = task.await;
        }
        match result {
            Ok(result) => result,
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "host output did not finish before deadline",
            )),
        }
    }
}

impl Drop for HostOutput {
    fn drop(&mut self) {
        // Drop cannot wait. Actual fd/flag cleanup occurs when Tokio next runs
        // the aborted task. Use finish when synchronous cleanup is required.
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "host output writer closed")
}

fn copy_error(error: &io::Error) -> io::Error {
    match error.raw_os_error() {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(error.kind(), error.to_string()),
    }
}

async fn write_all(output: &AsyncFd<NonblockingFd>, bytes: &[u8]) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let mut ready = output.writable().await?;
        let end = bytes.len().min(offset.saturating_add(64 * 1024));
        match ready.try_io(|fd| {
            let n = unsafe {
                libc::write(
                    fd.as_raw_fd(),
                    bytes[offset..end].as_ptr().cast(),
                    end - offset,
                )
            };
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }) {
            Err(_) => continue,
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
            Ok(Err(error)) => return Err(error),
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "host output wrote zero bytes",
                ));
            }
            Ok(Ok(n)) => offset += n,
        }
        // Readiness can remain true indefinitely on a fast host. Explicitly
        // yield after bounded work rather than monopolizing the main runtime.
        tokio::task::yield_now().await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus;
    use std::os::unix::net::UnixStream;

    fn flags(fd: RawFd) -> libc::c_int {
        let result = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(result >= 0);
        // Darwin additionally exposes its immutable FWASWRITTEN bit after
        // write(2); F_SETFL cannot clear it. Check the public status flags.
        result
            & (libc::O_ACCMODE | libc::O_NONBLOCK | libc::O_APPEND | libc::O_ASYNC | libc::O_SYNC)
    }

    fn pair() -> (UnixStream, AsyncFd<UnixStream>) {
        let (writer, reader) = UnixStream::pair().unwrap();
        let size: libc::c_int = 4096;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    writer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        reader.set_nonblocking(true).unwrap();
        (
            writer,
            AsyncFd::with_interest(reader, Interest::READABLE).unwrap(),
        )
    }

    async fn read_bytes(reader: &AsyncFd<UnixStream>, length: usize) -> Vec<u8> {
        let mut bytes = vec![0; length];
        let mut offset = 0;
        while offset < length {
            let mut ready = reader.readable().await.unwrap();
            match ready.try_io(|fd| {
                let n = unsafe {
                    libc::read(
                        fd.as_raw_fd(),
                        bytes[offset..].as_mut_ptr().cast(),
                        length - offset,
                    )
                };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            }) {
                Err(_) => continue,
                Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => continue,
                Ok(result) => {
                    let n = result.unwrap();
                    assert_ne!(n, 0);
                    offset += n;
                }
            }
        }
        bytes
    }

    #[tokio::test]
    async fn large_frames_order_and_runtime_progress_while_blocked() {
        let (writer, reader) = pair();
        let original = flags(writer.as_raw_fd());
        let (bus, _data, mut controls) = bus::channel();
        let mut output = HostOutput::new(writer.as_raw_fd(), bus).unwrap();
        assert_ne!(flags(writer.as_raw_fd()) & libc::O_NONBLOCK, 0);
        let first = vec![0x31; 1024 * 1024];
        let second = vec![0x72; 1024 * 1024];
        let reset = b"reset";
        output.send_frame(first.clone()).unwrap();
        // This timer must run on the same current-thread runtime even though
        // the peer isn't reading and the first frame cannot possibly fit.
        assert!(
            tokio::time::timeout(Duration::from_millis(25), controls.recv())
                .await
                .is_err()
        );
        let expected = [first, second.clone(), reset.to_vec()].concat();
        let reader_task =
            tokio::spawn(async move { read_bytes(&reader, expected.len()).await == expected });
        let event = tokio::time::timeout(Duration::from_secs(3), controls.recv())
            .await
            .unwrap();
        assert!(matches!(
            event,
            Some(Control::FrameWritten { result: Ok(()), .. })
        ));
        output.send_frame(second).unwrap();
        // Queue reset while the second frame is still in flight.
        output.finish(reset, Duration::from_secs(3)).await.unwrap();
        assert!(matches!(
            controls.recv().await,
            Some(Control::FrameWritten { result: Ok(()), .. })
        ));
        assert!(
            tokio::time::timeout(Duration::from_secs(3), reader_task)
                .await
                .unwrap()
                .unwrap()
        );
        assert_eq!(flags(writer.as_raw_fd()), original);
    }

    #[tokio::test]
    async fn blocked_finish_cancels_and_restores_flags() {
        let (writer, _reader) = pair();
        let original = flags(writer.as_raw_fd());
        let (bus, _data, mut controls) = bus::channel();
        let mut output = HostOutput::new(writer.as_raw_fd(), bus).unwrap();
        output.send_frame(vec![1; 1024 * 1024]).unwrap();
        let error = output
            .finish(b"reset", Duration::from_millis(25))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(flags(writer.as_raw_fd()), original);
        assert!(controls.try_recv().is_err());
        assert_eq!(
            output.send_frame(vec![2]).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[tokio::test]
    async fn drop_cleans_up_on_runtime_yield() {
        let (writer, _reader) = pair();
        let original = flags(writer.as_raw_fd());
        let (bus, _data, _controls) = bus::channel();
        let mut output = HostOutput::new(writer.as_raw_fd(), bus).unwrap();
        output.send_frame(vec![1; 1024 * 1024]).unwrap();
        let task = output.task.as_ref().unwrap().abort_handle();
        drop(output);
        tokio::task::yield_now().await;
        assert!(task.is_finished());
        assert_eq!(flags(writer.as_raw_fd()), original);
    }

    #[tokio::test]
    async fn preserves_preexisting_nonblocking_flags() {
        let (writer, _reader) = pair();
        writer.set_nonblocking(true).unwrap();
        let original = flags(writer.as_raw_fd());
        let (bus, _data, _controls) = bus::channel();
        let mut output = HostOutput::new(writer.as_raw_fd(), bus).unwrap();
        output
            .finish(b"reset", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(flags(writer.as_raw_fd()), original);
    }

    #[tokio::test]
    async fn write_error_is_exact_and_reported_once() {
        let (writer, reader) = pair();
        let original = flags(writer.as_raw_fd());
        let (bus, _data, mut controls) = bus::channel();
        let mut output = HostOutput::new(writer.as_raw_fd(), bus).unwrap();
        drop(reader);
        output.send_frame(vec![1; 1024]).unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), controls.recv())
            .await
            .unwrap();
        let Some(Control::FrameWritten {
            result: Err(error), ..
        }) = event
        else {
            panic!("expected frame write failure");
        };
        assert_eq!(error.raw_os_error(), Some(libc::EPIPE));
        let finish_error = output
            .finish(b"reset", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(finish_error.raw_os_error(), error.raw_os_error());
        assert!(controls.try_recv().is_err());
        assert_eq!(flags(writer.as_raw_fd()), original);
    }

    #[tokio::test]
    async fn full_queue_does_not_replace_accepted_frame() {
        let (writer, reader) = pair();
        let (bus, _data, mut controls) = bus::channel();
        let mut output = HostOutput::new(writer.as_raw_fd(), bus).unwrap();
        // No yield: the worker has not consumed the channel's one slot.
        output.send_frame(b"accepted".to_vec()).unwrap();
        assert_eq!(
            output.send_frame(b"rejected".to_vec()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        output
            .finish(b"reset", Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(read_bytes(&reader, 13).await, b"acceptedreset");
        assert!(matches!(
            controls.recv().await,
            Some(Control::FrameWritten { result: Ok(()), .. })
        ));
        assert!(controls.try_recv().is_err());
    }
}
