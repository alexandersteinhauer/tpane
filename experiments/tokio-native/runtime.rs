//! UI/terminal state is !Send and stays in Runtime::block_on on the caller.
//! Send-only I/O tasks have one independent worker, so rendering cannot stop
//! PTY draining. Do not replace this with a current-thread runtime.
pub fn new() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("tpane-io")
        .enable_all()
        .build()
        .expect("Tokio runtime")
}

#[cfg(test)]
mod tests {
    use std::{rc::Rc, sync::mpsc, thread, time::Duration};

    #[test]
    fn io_progresses_while_non_send_ui_stays_on_its_calling_thread() {
        let runtime = super::new();
        let owner = thread::current().id();
        let non_send_state = Rc::new(42);
        runtime.block_on(async {
            let (send, receive) = mpsc::channel();
            let io = tokio::spawn(async move {
                // Exercise the I/O reactor, not just a scheduled closure.
                use std::os::{fd::OwnedFd, unix::net::UnixStream};
                use tokio::io::{Interest, unix::AsyncFd};
                let (fd, mut peer) = UnixStream::pair().unwrap();
                fd.set_nonblocking(true).unwrap();
                let fd = AsyncFd::with_interest(OwnedFd::from(fd), Interest::READABLE).unwrap();
                std::io::Write::write_all(&mut peer, b"x").unwrap();
                let _ready = fd.readable().await.unwrap();
                send.send(thread::current().id()).unwrap();
            });
            // Deliberately occupy the caller, modeling synchronous UI work.
            // A current-thread runtime cannot drive the task/reactor here.
            assert_ne!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), owner);
            io.await.unwrap();
            assert_eq!(thread::current().id(), owner);
            assert_eq!(*non_send_state, 42);
        });
    }
}
