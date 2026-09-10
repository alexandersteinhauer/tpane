//! A macOS PTY and its login shell.
//!
//! Cleanup enumerates the entire terminal session (jobs have distinct process
//! groups), plus descendants that created a new session. This is best effort:
//! macOS has no unprivileged equivalent of a killable process cgroup. A daemon
//! that both detaches and reparents before discovery, a privileged child, or a
//! process stuck in the kernel can escape cleanup. Only our direct child can be
//! reaped; orphaned grandchildren are reaped by launchd.

use anyhow::{Context, Result, ensure};
use portable_pty::{
    Child, CommandBuilder, ExitStatus, MasterPty, PtyPair, PtySize, native_pty_system,
};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Session {
    master: Box<dyn MasterPty + Send>,
    writer: Option<Box<dyn Write + Send>>,
    child: Box<dyn Child + Send + Sync>,
    shell_pid: libc::pid_t,
    known: HashMap<libc::pid_t, Process>,
    exit_status: Option<ExitStatus>,
}

impl Session {
    #[cfg(test)]
    pub fn spawn(shell: &Path, rows: u16, cols: u16) -> Result<Self> {
        Self::spawn_in(
            shell,
            rows,
            cols,
            &std::env::current_dir().context("reading current directory")?,
        )
    }

    pub fn spawn_in(shell: &Path, rows: u16, cols: u16, cwd: &Path) -> Result<Self> {
        let size = size(rows, cols)?;
        let pair = open_pty(size)?;
        // Acquire this before spawning so a writer failure cannot orphan a child.
        let writer = pair.master.take_writer().context("opening PTY writer")?;
        let mut command = CommandBuilder::new(shell);
        command.arg("-l");
        // portable-pty otherwise defaults to HOME.
        command.cwd(cwd);
        command.env("SHELL", shell);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        // Do not advertise the enclosing emulator's private protocols/sockets.
        // Keep general user environment, including SSH_AUTH_SOCK and locale.
        for key in [
            "TERM_PROGRAM",
            "TERM_PROGRAM_VERSION",
            "TERM_SESSION_ID",
            "LC_TERMINAL",
            "LC_TERMINAL_VERSION",
            "TERMINAL_EMULATOR",
            "GHOSTTY_RESOURCES_DIR",
            "GHOSTTY_BIN_DIR",
            "GHOSTTY_SHELL_INTEGRATION",
            "KITTY_WINDOW_ID",
            "KITTY_LISTEN_ON",
            "KITTY_PID",
            "KITTY_INSTALLATION_DIR",
            "WEZTERM_PANE",
            "WEZTERM_UNIX_SOCKET",
            "WEZTERM_EXECUTABLE",
            "WEZTERM_EXEC_DIR",
            "ITERM_SESSION_ID",
            "ITERM_PROFILE",
            "VTE_VERSION",
            "WINDOWID",
            "TMUX",
            "TMUX_PANE",
            "STY",
            "WINDOW",
            "TERMCAP",
            "TERMINFO",
            "TERMINFO_DIRS",
            "COLUMNS",
            "LINES",
        ] {
            command.env_remove(key);
        }
        // The Unix backend calls setsid(), attaches the controlling terminal,
        // and connects all three standard streams to the slave before exec.
        let child = pair
            .slave
            .spawn_command(command)
            .with_context(|| format!("spawning login shell {}", shell.display()))?;
        drop(pair.slave); // Retaining a slave prevents readers from seeing EOF.
        let shell_pid = child.process_id().expect("macOS PTY child has a PID") as libc::pid_t;
        let mut known = HashMap::new();
        if let Some(process) = Process::read(shell_pid) {
            known.insert(shell_pid, process);
        }
        Ok(Self {
            master: pair.master,
            writer: Some(writer),
            child,
            shell_pid,
            known,
            exit_status: None,
        })
    }

    pub fn cwd(&self) -> Option<PathBuf> {
        if self.exit_status.is_some() {
            return None;
        }
        let shell = self.known.get(&self.shell_pid)?;
        if !shell.is_running() {
            return None;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
        let len = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        let read = unsafe {
            libc::proc_pidinfo(
                self.shell_pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                info.as_mut_ptr().cast(),
                len,
            )
        };
        // Check birth time on both sides of the query to reject PID reuse.
        if read != len || !shell.is_running() {
            return None;
        }
        let info = unsafe { info.assume_init() };
        let path = info.pvi_cdir.vip_path.as_flattened();
        let end = path.iter().position(|&byte| byte == 0)?;
        let path = PathBuf::from(OsString::from_vec(
            path[..end].iter().map(|&byte| byte as u8).collect(),
        ));
        path.is_absolute().then_some(path)
    }

    pub fn has_foreground_job(&self) -> bool {
        if self.exit_status.is_some() {
            return false;
        }
        if let Some(shell) = Process::read(self.shell_pid)
            && (shell.zombie
                || self
                    .known
                    .get(&self.shell_pid)
                    .is_some_and(|known| !known.same_process(&shell)))
        {
            return false;
        }
        let Some(fd) = self.master.as_raw_fd() else {
            return true;
        };
        let foreground = unsafe { libc::tcgetpgrp(fd) };
        foreground <= 0 || foreground != self.shell_pid
    }

    /// Readers own a duplicate descriptor. Callers must stop their reader worker
    /// at EOF/error; dropping Session cannot directly close that duplicate.
    pub fn reader(&self) -> Result<Box<dyn Read + Send>> {
        self.master.try_clone_reader().context("cloning PTY reader")
    }

    pub fn take_writer(&mut self) -> Result<Box<dyn Write + Send>> {
        self.writer.take().context("PTY writer already taken")
    }

    #[cfg(test)]
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer
            .as_mut()
            .expect("writer exists until drop")
            .write_all(bytes)
            .context("writing to PTY")
    }

    pub fn resize(&self, rows: u16, cols: u16) -> Result<()> {
        self.master
            .resize(size(rows, cols)?)
            .context("resizing PTY")
    }

    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        if self.exit_status.is_none() {
            self.exit_status = self.child.try_wait().context("polling login shell")?;
            if self.exit_status.is_some() {
                // Retain identities immediately after reaping, while orphaned
                // jobs still identify this session. Future cleanup uses these
                // identities, not an indefinitely reusable shell PID.
                self.discover(true);
            }
        }
        Ok(self.exit_status.clone())
    }

    fn discover(&mut self, allow_unanchored_session: bool) {
        let processes = process_snapshot();
        let shell = processes.iter().find(|p| p.pid == self.shell_pid);
        let shell_reused = shell.is_some_and(|p| {
            self.known
                .get(&self.shell_pid)
                .is_some_and(|old| !old.same_process(p))
        });
        let anchored = processes.iter().any(|p| {
            p.sid == self.shell_pid
                && self
                    .known
                    .get(&p.pid)
                    .is_some_and(|old| old.same_process(p))
        });
        let include_session = !shell_reused && (anchored || allow_unanchored_session);
        // Iterate to closure: snapshot order is not necessarily parent first.
        loop {
            let mut added = false;
            for process in &processes {
                if process.pid <= 1 || process.pid == std::process::id() as libc::pid_t {
                    continue;
                }
                let descendant = self.known.get(&process.ppid).is_some_and(|parent| {
                    processes
                        .iter()
                        .any(|p| p.pid == parent.pid && p.same_process(parent))
                });
                if (include_session && process.sid == self.shell_pid) || descendant {
                    let new = self
                        .known
                        .get(&process.pid)
                        .is_none_or(|old| !old.same_process(process));
                    if new {
                        self.known.insert(process.pid, *process);
                        added = true;
                    }
                }
            }
            if !added {
                break;
            }
        }
    }

    fn signal_known(&self, signal: libc::c_int) {
        // Until we reap it, the owned child's PID cannot be reused. This also
        // provides shell cleanup if libproc inspection is unavailable.
        if self.exit_status.is_none() {
            unsafe {
                libc::kill(self.shell_pid, signal);
            }
        }
        for process in self.known.values() {
            if process.pid != self.shell_pid {
                process.signal(signal);
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // portable-pty's writer Drop writes newline + VEOF. A full/stopped PTY
        // can block that write forever. Duplicates share these file status flags;
        // change them only now, when the session is shutting down.
        if let Some(fd) = self.master.as_raw_fd() {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags != -1 {
                    libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
        }

        // Freeze the discovered tree before terminating parents, preserving
        // ancestry while finding children in other process groups/sessions.
        // Re-scan a bounded number of times to catch concurrent forks.
        let freeze_deadline = Instant::now() + Duration::from_millis(100);
        for pass in 0..8 {
            let before = self.known.len();
            self.discover(self.exit_status.is_none());
            self.signal_known(libc::SIGSTOP);
            if (pass > 0 && self.known.len() == before) || Instant::now() >= freeze_deadline {
                break;
            }
        }
        self.signal_known(libc::SIGTERM);
        self.signal_known(libc::SIGCONT); // Stopped jobs must get to handle TERM.

        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if self.exit_status.is_none()
                && let Ok(Some(status)) = self.child.try_wait()
            {
                self.exit_status = Some(status);
            }
            self.discover(false);
            if !self.known.values().any(Process::is_running) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.signal_known(libc::SIGKILL);
        // Never call Child::wait(): even SIGKILL cannot guarantee a prompt exit.
        let reap_deadline = Instant::now() + Duration::from_millis(200);
        while self.exit_status.is_none() && Instant::now() < reap_deadline {
            match self.child.try_wait() {
                Ok(Some(status)) => self.exit_status = Some(status),
                Err(_) => break,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        drop(self.writer.take());
    }
}

fn open_pty(size: PtySize) -> Result<PtyPair> {
    // Concurrent macOS PTY allocation can fail; serialize allocation, not I/O or spawning.
    static ALLOCATION: Mutex<()> = Mutex::new(());
    let _guard = ALLOCATION
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    native_pty_system().openpty(size).context("opening PTY")
}

fn size(rows: u16, cols: u16) -> Result<PtySize> {
    ensure!(rows > 0 && cols > 0, "PTY dimensions must be nonzero");
    Ok(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })
}

#[derive(Clone, Copy)]
struct Process {
    pid: libc::pid_t,
    ppid: libc::pid_t,
    sid: libc::pid_t,
    start: (u64, u64),
    zombie: bool,
}

impl Process {
    fn read(pid: libc::pid_t) -> Option<Self> {
        if pid <= 1 {
            return None;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let len = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // libproc fills exactly this versioned structure on success.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                len,
            )
        };
        if read != len {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(Self {
            pid,
            ppid: info.pbi_ppid as libc::pid_t,
            sid: unsafe { libc::getsid(pid) },
            start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            zombie: info.pbi_status == libc::SZOMB,
        })
    }

    fn same_process(&self, other: &Self) -> bool {
        self.pid == other.pid && self.start == other.start
    }

    fn is_running(&self) -> bool {
        Self::read(self.pid).is_some_and(|p| self.same_process(&p) && !p.zombie)
    }

    fn signal(&self, signal: libc::c_int) {
        // Birth time prevents signaling most PID reuse cases. macOS has no
        // pidfd: a tiny check-to-kill race remains, so never signal a whole group.
        if self.pid != std::process::id() as libc::pid_t && self.is_running() {
            unsafe {
                libc::kill(self.pid, signal);
            }
        }
    }
}

fn process_snapshot() -> Vec<Process> {
    // PROC_ALL_PIDS from <sys/proc_info.h> is not exported by libc.
    const PROC_ALL_PIDS: u32 = 1;
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    // Leave headroom for forks between the sizing and reading calls. Bounded
    // retries in Drop handle additional churn without unbounded allocation.
    let count = (bytes as usize / std::mem::size_of::<libc::pid_t>() + 256).min(1_048_576);
    let mut pids = vec![0 as libc::pid_t; count];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    pids.truncate(bytes as usize / std::mem::size_of::<libc::pid_t>());
    pids.into_iter().filter_map(Process::read).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::{self, Receiver};

    struct Terminal {
        session: Session,
        output: Receiver<Vec<u8>>,
        received: String,
    }

    impl Terminal {
        fn new() -> Self {
            // sh avoids personal zsh/bash interactive configuration; -l still
            // exercises the public login-shell path.
            Self::with_session(Session::spawn(Path::new("/bin/sh"), 24, 80).unwrap())
        }

        fn with_session(session: Session) -> Self {
            let mut reader = session.reader().unwrap();
            let (tx, output) = mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0; 4096];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
            });
            let mut terminal = Self {
                session,
                output,
                received: String::new(),
            };
            terminal.send("stty -echo; set +H; PS1=''; printf '__%s__\\n' READY\n");
            terminal.until("__READY__");
            terminal
        }

        fn send(&mut self, command: &str) {
            if let Err(error) = self.session.write(command.as_bytes()) {
                let status = self.session.try_wait();
                let process =
                    Process::read(self.session.shell_pid).map(|p| (p.pid, p.ppid, p.sid, p.zombie));
                panic!(
                    "PTY write: {error:#}; status: {status:?}; process: {process:?}; output: {:?}",
                    self.received
                );
            }
        }

        fn until(&mut self, marker: &str) -> String {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.received.contains(marker) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let bytes = self.output.recv_timeout(remaining).unwrap_or_else(|e| {
                    panic!("waiting for {marker:?}: {e}; output: {:?}", self.received)
                });
                self.received.push_str(&String::from_utf8_lossy(&bytes));
            }
            let end = self.received.find(marker).unwrap() + marker.len();
            self.received.drain(..end).collect()
        }

        fn job_pid(&mut self) -> libc::pid_t {
            self.until("__PID__");
            let line = self.until("__END__");
            line.trim_end_matches("__END__").trim().parse().unwrap()
        }
    }

    fn assert_gone(process: Process) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while process.is_running() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !process.is_running(),
            "process {} survived cleanup",
            process.pid
        );
    }

    #[test]
    fn concurrent_allocations_keep_independent_sizes() {
        for _ in 0..20 {
            let barrier = std::sync::Barrier::new(8);
            std::thread::scope(|scope| {
                for rows in 1..=8 {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        let pair = open_pty(size(rows, 80).unwrap());
                        barrier.wait();
                        let pair = pair.unwrap();
                        assert_eq!(pair.master.get_size().unwrap().rows, rows);
                    });
                }
            });
        }
    }

    #[test]
    fn output_environment_login_and_cwd() {
        let mut terminal = Terminal::new();
        terminal.send("printf 'ENV=%s/%s\\n' \"$TERM\" \"$COLORTERM\"; pwd; shopt -q login_shell && printf 'LOGIN=yes\\n'; printf '__%s__\\n' DONE\n");
        let output = terminal.until("__DONE__");
        assert!(output.contains("ENV=xterm-256color/truecolor"), "{output}");
        assert!(output.contains("LOGIN=yes"), "{output}");
        assert!(
            output.contains(std::env::current_dir().unwrap().to_str().unwrap()),
            "{output}"
        );
        assert!(terminal.session.try_wait().unwrap().is_none());
    }

    #[test]
    fn spawn_in_uses_requested_directory() {
        let cwd = std::env::temp_dir().canonicalize().unwrap();
        let session = Session::spawn_in(Path::new("/bin/sh"), 24, 80, &cwd).unwrap();
        let mut terminal = Terminal::with_session(session);
        assert_eq!(terminal.session.cwd(), Some(cwd.clone()));
        terminal.send("pwd -P; printf '__%s__\\n' DIRECTORY\n");
        let output = terminal.until("__DIRECTORY__");
        assert!(output.contains(cwd.to_str().unwrap()), "{output}");
    }

    #[test]
    fn cwd_tracks_cd_and_rejects_unavailable_identity() {
        let mut terminal = Terminal::new();
        terminal.send("cd /; printf '__%s__\\n' CHANGED\n");
        terminal.until("__CHANGED__");
        assert_eq!(terminal.session.cwd(), Some(PathBuf::from("/")));

        let pid = terminal.session.shell_pid;
        let identity = terminal.session.known.remove(&pid).unwrap();
        assert_eq!(terminal.session.cwd(), None);
        let mut wrong_identity = identity;
        wrong_identity.start.0 = wrong_identity.start.0.wrapping_add(1);
        terminal.session.known.insert(pid, wrong_identity);
        assert_eq!(terminal.session.cwd(), None);
        terminal.session.known.insert(pid, identity);

        terminal.session.shell_pid = libc::pid_t::MAX;
        terminal.session.known.insert(
            libc::pid_t::MAX,
            Process {
                pid: libc::pid_t::MAX,
                ..identity
            },
        );
        let cwd = terminal.session.cwd();
        terminal.session.shell_pid = pid;
        terminal.session.known.remove(&libc::pid_t::MAX);
        assert_eq!(cwd, None);
    }

    #[test]
    fn resize_reaches_slave() {
        let mut terminal = Terminal::new();
        terminal.session.resize(37, 101).unwrap();
        terminal.send("stty size; printf '__%s__\\n' SIZED\n");
        let output = terminal.until("__SIZED__");
        assert!(output.contains("37 101"), "{output}");
        assert!(terminal.session.resize(0, 80).is_err());
    }

    #[test]
    fn foreground_job_exits_and_ctrl_c_returns_to_shell() {
        let mut terminal = Terminal::new();
        assert!(!terminal.session.has_foreground_job());
        terminal.send("/bin/sleep 0.05; printf '__%s__\\n' RETURNED\n");
        terminal.until("__RETURNED__");
        terminal.send("/bin/sleep 60\n");
        // Wait for the kernel's foreground group rather than guessing a delay.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !terminal.session.has_foreground_job() {
            assert!(Instant::now() < deadline, "job did not enter foreground");
            std::thread::sleep(Duration::from_millis(5));
        }
        terminal.session.write(&[3]).unwrap();
        terminal.send("printf '__%s__\\n' INTERRUPTED\n");
        terminal.until("__INTERRUPTED__");
        assert!(terminal.session.try_wait().unwrap().is_none());
        assert!(!terminal.session.has_foreground_job());
    }

    #[test]
    fn drop_kills_background_job_in_another_group_even_if_stopped() {
        let mut terminal = Terminal::new();
        terminal
            .send("(trap '' HUP TERM; exec /bin/sleep 60) & printf '__PID__%s__END__\\n' \"$!\"\n");
        let pid = terminal.job_pid();
        let process = Process::read(pid).unwrap();
        assert_eq!(process.sid, terminal.session.shell_pid);
        assert_ne!(unsafe { libc::getpgid(pid) }, terminal.session.shell_pid);
        process.signal(libc::SIGSTOP);
        let shell = Process::read(terminal.session.shell_pid).unwrap();
        let start = Instant::now();
        drop(terminal);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_gone(process);
        assert_gone(shell);
    }

    #[test]
    fn drop_after_shell_exit_kills_orphaned_background_job() {
        let mut terminal = Terminal::new();
        terminal.send("(trap '' HUP TERM; exec /bin/sleep 60) </dev/null >/dev/null 2>&1 & printf '__PID__%s__END__\\n' \"$!\"; disown; exit 7\n");
        let process = Process::read(terminal.job_pid()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = terminal.session.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "shell did not exit");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(status.exit_code(), 7);
        assert!(!terminal.session.has_foreground_job());
        assert_eq!(terminal.session.cwd(), None);
        drop(terminal);
        assert_gone(process);
    }

    #[test]
    fn drop_kills_descendant_that_created_a_new_session() {
        let mut terminal = Terminal::new();
        // The intermediate parent stays alive to retain discoverable ancestry.
        // A double-forked daemon already reparented to launchd is not discoverable.
        terminal.send(concat!(
            "/usr/bin/perl -MPOSIX -e '$p = fork(); die unless defined $p; ",
            "if ($p) { waitpid($p, 0); } else { ",
            "POSIX::setsid() >= 0 or die; $SIG{HUP} = $SIG{TERM} = \"IGNORE\"; ",
            "$| = 1; print \"__PID__$$ __END__\\n\"; sleep 60; }' &\n",
        ));
        let process = Process::read(terminal.job_pid()).unwrap();
        assert_ne!(process.sid, terminal.session.shell_pid);
        drop(terminal);
        assert_gone(process);
    }

    #[test]
    fn drop_is_bounded_with_a_full_input_queue() {
        let terminal = Terminal::new();
        let shell = Process::read(terminal.session.shell_pid).unwrap();
        shell.signal(libc::SIGSTOP);
        let fd = terminal.session.master.as_raw_fd().unwrap();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let bytes = vec![b'x'; 1024 * 1024];
        // Bound the fill attempt too; the shell is stopped and cannot consume.
        for _ in 0..16 {
            let result = unsafe { libc::write(fd, bytes.as_ptr().cast(), bytes.len()) };
            if result < 0 {
                break;
            }
        }
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFL, flags) }, 0);
        let start = Instant::now();
        drop(terminal);
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_gone(shell);
    }

    #[test]
    fn rejects_invalid_shell_and_dimensions() {
        assert!(Session::spawn(Path::new("/does/not/exist"), 24, 80).is_err());
        assert!(Session::spawn(Path::new("/bin/sh"), 0, 80).is_err());
    }
}
