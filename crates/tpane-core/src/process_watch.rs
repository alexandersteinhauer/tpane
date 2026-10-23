//! Evented child-exit notification only: the owner of each Child must reap it.
use std::io::{self, Result};

use crate::{Id, bus::Bus};

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use crate::bus::Control;
    use std::{
        collections::HashMap,
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        sync::{Arc, Mutex},
    };
    use tokio::{
        io::{Interest, unix::AsyncFd},
        task::JoinHandle,
    };

    pub struct ProcessWatch {
        queue: Arc<AsyncFd<OwnedFd>>,
        watched: Arc<Mutex<HashMap<libc::pid_t, Id>>>,
        bus: Bus,
        task: JoinHandle<()>,
    }

    // All kevent calls have a zero timeout, including registration/deletion.
    fn change(fd: libc::c_int, pid: libc::pid_t, id: Id, flags: u16) -> Result<()> {
        let event = libc::kevent {
            ident: pid as libc::uintptr_t,
            filter: libc::EVFILT_PROC,
            flags,
            fflags: libc::NOTE_EXIT,
            data: 0,
            udata: id as usize as *mut libc::c_void,
        };
        let zero = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            let n = unsafe { libc::kevent(fd, &event, 1, std::ptr::null_mut(), 0, &zero) };
            if n >= 0 {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    impl ProcessWatch {
        pub fn new(bus: Bus) -> Result<Self> {
            let fd = unsafe { libc::kqueue() };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
                return Err(io::Error::last_os_error());
            }
            // A kqueue fd supports read readiness, not EVFILT_WRITE.
            let queue = Arc::new(AsyncFd::with_interest(fd, Interest::READABLE)?);
            let watched = Arc::new(Mutex::new(HashMap::new()));
            let task_queue = queue.clone();
            let task_watched = watched.clone();
            let task_bus = bus.clone();
            let task = tokio::spawn(async move {
                loop {
                    let mut ready = match task_queue.readable().await {
                        Ok(ready) => ready,
                        Err(_) => return,
                    };
                    // Serialize draining with watch/unwatch so an already dequeued
                    // event cannot be emitted after unwatch returns.
                    let result = ready.try_io(|queue| {
                        let mut watched = task_watched.lock().unwrap();
                        let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
                        let zero = libc::timespec {
                            tv_sec: 0,
                            tv_nsec: 0,
                        };
                        let n = unsafe {
                            libc::kevent(
                                queue.as_raw_fd(),
                                std::ptr::null(),
                                0,
                                events.as_mut_ptr(),
                                events.len() as i32,
                                &zero,
                            )
                        };
                        if n < 0 {
                            return Err(io::Error::last_os_error());
                        }
                        if n == 0 {
                            return Err(io::ErrorKind::WouldBlock.into());
                        }
                        for event in &events[..n as usize] {
                            let pid = event.ident as libc::pid_t;
                            let id = event.udata as usize as Id;
                            if event.filter == libc::EVFILT_PROC
                                && event.fflags & libc::NOTE_EXIT != 0
                                && watched.get(&pid) == Some(&id)
                            {
                                watched.remove(&pid);
                                let _ = task_bus.control.send(Control::ChildExited(id));
                            }
                        }
                        Ok(())
                    });
                    match result {
                        Ok(Err(error)) if error.kind() != io::ErrorKind::Interrupted => return,
                        _ => {}
                    }
                    tokio::task::yield_now().await;
                }
            });
            Ok(Self {
                queue,
                watched,
                bus,
                task,
            })
        }

        pub fn watch(&mut self, pid: libc::pid_t, id: Id) -> Result<()> {
            if pid <= 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "expected a child PID",
                ));
            }
            let mut watched = self.watched.lock().unwrap();
            match change(
                self.queue.as_raw_fd(),
                pid,
                id,
                libc::EV_ADD | libc::EV_ONESHOT,
            ) {
                Ok(()) => {
                    watched.insert(pid, id);
                    Ok(())
                }
                Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
                    watched.remove(&pid);
                    // Exit can win registration. The owner validates this hint
                    // using its owned Child, never waitpid here.
                    self.bus
                        .control
                        .send(Control::ChildExited(id))
                        .map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "control receiver closed")
                        })
                }
                Err(error) => Err(error),
            }
        }

        pub fn unwatch(&mut self, pid: libc::pid_t) -> Result<()> {
            let mut watched = self.watched.lock().unwrap();
            match change(self.queue.as_raw_fd(), pid, 0, libc::EV_DELETE) {
                Ok(()) => {}
                Err(error) if matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => {}
                Err(error) => return Err(error),
            }
            watched.remove(&pid);
            Ok(())
        }
    }

    impl Drop for ProcessWatch {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
}

#[cfg(target_os = "macos")]
pub use platform::ProcessWatch;

#[cfg(not(target_os = "macos"))]
pub struct ProcessWatch;

#[cfg(not(target_os = "macos"))]
impl ProcessWatch {
    pub fn new(_: Bus) -> Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "process watching requires macOS kqueue",
        ))
    }
    pub fn watch(&mut self, _: libc::pid_t, _: Id) -> Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
    pub fn unwatch(&mut self, _: libc::pid_t) -> Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use crate::bus::{self, Control};
    use std::{
        process::{Child, Command},
        time::Duration,
    };

    struct OwnedChild(Child);
    impl OwnedChild {
        fn spawn(program: &str, args: &[&str]) -> Self {
            Self(Command::new(program).args(args).spawn().unwrap())
        }
        fn pid(&self) -> libc::pid_t {
            self.0.id() as libc::pid_t
        }
    }
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[tokio::test]
    async fn quick_exit_registration_race() {
        let (bus, _data, mut controls) = bus::channel();
        let mut watcher = ProcessWatch::new(bus).unwrap();
        for id in 1..=32 {
            let mut child = OwnedChild::spawn("/usr/bin/true", &[]);
            // Alternate a real registration race with deterministic ESRCH.
            if id % 2 == 0 {
                child.0.wait().unwrap();
            }
            watcher.watch(child.pid(), id).unwrap();
            let event = tokio::time::timeout(Duration::from_secs(3), controls.recv())
                .await
                .unwrap();
            assert!(matches!(event, Some(Control::ChildExited(actual)) if actual == id));
            child.0.wait().unwrap();
            watcher.unwatch(child.pid()).unwrap();
            assert!(controls.try_recv().is_err());
        }
    }

    #[tokio::test]
    async fn live_exit_exact_id_and_unwatch() {
        let (bus, _data, mut controls) = bus::channel();
        let mut watcher = ProcessWatch::new(bus).unwrap();
        let mut child = OwnedChild::spawn("/bin/sleep", &["30"]);
        let mut ignored = OwnedChild::spawn("/bin/sleep", &["30"]);
        let id = 0xfedc_ba98_7654_3210;
        watcher.watch(child.pid(), id).unwrap();
        watcher.watch(ignored.pid(), 42).unwrap();
        watcher.unwatch(ignored.pid()).unwrap();
        ignored.0.kill().unwrap();
        ignored.0.wait().unwrap();
        child.0.kill().unwrap();
        let event = tokio::time::timeout(Duration::from_secs(3), controls.recv())
            .await
            .unwrap();
        assert!(matches!(event, Some(Control::ChildExited(actual)) if actual == id));
        child.0.wait().unwrap();
        watcher.unwatch(child.pid()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), controls.recv())
                .await
                .is_err()
        );
    }
}
