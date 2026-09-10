//! Black-box tests: the real binary owns a slave PTY, and Ghostty interprets
//! its output just as an enclosing terminal would. No source modules are imported.
#![cfg(target_os = "macos")]

use std::{
    io::{self, Read, Write},
    path::Path,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use libghostty_vt::{
    Terminal, TerminalOptions,
    screen::Screen,
    terminal::{Mode, Point, PointCoordinate},
};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, SlavePty, native_pty_system};

const TIMEOUT: Duration = Duration::from_secs(8);
const HOST_TEXT: &str = "original host screen";

// Process birth times make panic cleanup safe against PID reuse. A zombie is
// already dead; grandchildren are reaped by launchd rather than this test.
#[derive(Clone, Copy)]
struct Process {
    pid: libc::pid_t,
    birth: (u64, u64),
}

impl Process {
    fn read(pid: libc::pid_t) -> Option<Self> {
        if pid <= 1 {
            return None;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let n = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if n != size {
            return None;
        }
        let info = unsafe { info.assume_init() };
        (info.pbi_status != libc::SZOMB).then_some(Self {
            pid,
            birth: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        })
    }

    fn alive(self) -> bool {
        Self::read(self.pid).is_some_and(|p| p.birth == self.birth)
    }

    fn signal(self, signal: libc::c_int) {
        if self.alive() {
            unsafe { libc::kill(self.pid, signal) };
        }
    }
}

struct Host {
    master: Box<dyn MasterPty + Send>,
    // Keep a slave open so termios remains inspectable after tpane exits. We
    // deliberately use try_wait, not EOF, to detect process termination.
    _slave: Box<dyn SlavePty + Send>,
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    process: Process,
    dependents: Vec<Process>,
    terminal: Terminal<'static, 'static>,
    before: libc::termios,
    bytes: Vec<u8>,
    rows: u16,
    cols: u16,
    exited: bool,
    home: std::path::PathBuf,
}

fn termios(master: &dyn MasterPty) -> libc::termios {
    let mut attrs = std::mem::MaybeUninit::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd().unwrap(), attrs.as_mut_ptr()) },
        0
    );
    unsafe { attrs.assume_init() }
}

impl Host {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let home = std::env::temp_dir().join(format!(
            "tpane-terminal-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&home).unwrap();
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let before = termios(&*pair.master);
        let reader = pair.master.try_clone_reader().unwrap();
        let writer = pair.master.take_writer().unwrap();
        let fd = pair.master.as_raw_fd().unwrap();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 80,
            rows: 24,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(HOST_TEXT.as_bytes());
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_tpane"));
        command.args(["--shell", "/bin/sh"]);
        command.cwd(&home);
        command.env("HOME", &home);
        command.env("ENV", "/dev/null");
        command.env("BASH_ENV", "/dev/null");
        command.env("TERM", "xterm-256color");
        command.env("LC_ALL", "C");
        for name in ["COLUMNS", "LINES", "TMUX", "STY"] {
            command.env_remove(name);
        }
        let child = pair.slave.spawn_command(command).unwrap();
        let process = Process::read(child.process_id().unwrap() as libc::pid_t).unwrap();
        let mut host = Self {
            master: pair.master,
            _slave: pair.slave,
            reader,
            writer,
            child,
            process,
            dependents: Vec::new(),
            terminal,
            before,
            bytes: Vec::new(),
            rows: 24,
            cols: 80,
            exited: false,
            home,
        };
        // Split markers cannot match the initially echoed command. Subsequent
        // assertions inspect cells, never stale text in raw repaint sequences.
        host.send(b"stty -echo; set +H; PS1=''; PS2=''; printf '__%s:%s__\\n' SHELL \"$$\"\n");
        host.track_pid("__SHELL:");
        assert_eq!(host.terminal.active_screen().unwrap(), Screen::Alternate);
        host
    }

    fn send(&mut self, bytes: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match self.writer.write(remaining) {
                Ok(0) => panic!("PTY input closed"),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.pump(),
                Err(e) => panic!("PTY input: {e}"),
            }
            assert!(Instant::now() < deadline, "PTY input timed out");
        }
        self.writer.flush().unwrap();
    }

    fn pump(&mut self) {
        let mut poll = libc::pollfd {
            fd: self.master.as_raw_fd().unwrap(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, 20) };
        if ready < 0 {
            assert_eq!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::Interrupted
            );
            return;
        }
        if ready == 0 {
            return;
        }
        let mut bytes = [0; 65536];
        match self.reader.read(&mut bytes) {
            Ok(n) => {
                self.terminal.vt_write(&bytes[..n]);
                self.bytes.extend_from_slice(&bytes[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => panic!("PTY output: {e}"),
        }
    }

    fn visible(&self) -> String {
        let mut text = String::new();
        for y in 0..u32::from(self.rows) {
            let mut row = String::new();
            for x in 0..self.cols {
                let cp = self
                    .terminal
                    .grid_ref(Point::Active(PointCoordinate { x, y }))
                    .unwrap()
                    .cell()
                    .unwrap()
                    .codepoint()
                    .unwrap();
                row.push(if cp == 0 {
                    ' '
                } else {
                    char::from_u32(cp).unwrap_or('\u{fffd}')
                });
            }
            text.push_str(row.trim_end());
            text.push('\n');
        }
        text
    }

    fn until(&mut self, description: &str, predicate: impl Fn(&str) -> bool) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let text = self.visible();
            if predicate(&text) {
                return text;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {description}; visible screen:\n{text}\nraw tail: {:?}",
                String::from_utf8_lossy(&self.bytes[self.bytes.len().saturating_sub(2000)..])
            );
            self.pump();
        }
    }

    fn track_pid(&mut self, prefix: &str) -> Process {
        let parse = |text: &str| -> Option<libc::pid_t> {
            text.split_once(prefix)?.1.split_once("__")?.0.parse().ok()
        };
        let text = self.until(prefix, |s| parse(s).is_some());
        let pid = parse(&text).unwrap();
        let process = Process::read(pid).expect("reported process is alive");
        self.dependents.push(process);
        process
    }

    fn wait_exit(&mut self, expected: u32) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.pump();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.exited = true;
                assert_eq!(
                    status.exit_code(),
                    expected,
                    "{status:?}; {}",
                    self.visible()
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "tpane did not exit; {}",
                self.visible()
            );
        }
        self.until("host primary screen restored", |s| s.contains(HOST_TEXT));
        assert_eq!(self.terminal.active_screen().unwrap(), Screen::Primary);
        assert!(!self.terminal.is_mouse_tracking().unwrap());
        assert!(!self.terminal.mode(Mode::BRACKETED_PASTE).unwrap());
        assert!(!self.terminal.mode(Mode::FOCUS_EVENT).unwrap());
        assert!(self.terminal.mode(Mode::CURSOR_VISIBLE).unwrap());
        assert!(self.terminal.mode(Mode::WRAPAROUND).unwrap());
        assert!(
            self.bytes.windows(4).any(|b| b == b"\x1b[<u"),
            "keyboard protocol was not popped"
        );
        let after = termios(&*self.master);
        assert_eq!(
            after.c_iflag, self.before.c_iflag,
            "input flags not restored"
        );
        assert_eq!(
            after.c_oflag, self.before.c_oflag,
            "output flags not restored"
        );
        assert_eq!(
            after.c_cflag, self.before.c_cflag,
            "control flags not restored"
        );
        assert_eq!(
            after.c_lflag, self.before.c_lflag,
            "local flags not restored"
        );
        assert_eq!(
            after.c_cc, self.before.c_cc,
            "control characters not restored"
        );
        assert_eq!(after.c_ispeed, self.before.c_ispeed);
        assert_eq!(after.c_ospeed, self.before.c_ospeed);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // Also runs on assertion failures. No blocking wait or reader thread can
        // strand the test runner; tpane gets time to clean up its inner session.
        if !self.exited {
            self.process.signal(libc::SIGTERM);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    self.exited = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            if !self.exited {
                self.process.signal(libc::SIGKILL);
                let deadline = Instant::now() + Duration::from_millis(500);
                while Instant::now() < deadline {
                    if matches!(self.child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        for process in &self.dependents {
            process.signal(libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn command_returns_to_the_same_interactive_shell() {
    let mut host = Host::new();
    host.send(b"/bin/sh -c 'exit 7'; printf '__%s:%s__\\n' COMMAND \"$?\"\n");
    host.until("command exit status", |s| s.contains("__COMMAND:7__"));
    host.send(b"printf '__%s__\\n' NEXT_COMMAND\n");
    host.until("next shell command", |s| s.contains("__NEXT_COMMAND__"));
    host.send(b"exit 0\n");
    host.wait_exit(0);
}

#[test]
fn ctrl_c_interrupts_foreground_job_not_tpane() {
    let mut host = Host::new();
    host.send(b"/bin/sh -c 'printf \"__%s:%s__\\n\" JOB \"$$\"; exec /bin/sleep 60'; printf '__%s:%s__\\n' INTERRUPTED \"$?\"\n");
    let job = host.track_pid("__JOB:");
    host.send(&[3]);
    host.until("foreground SIGINT status", |s| {
        s.contains("__INTERRUPTED:130__")
    });
    assert!(!job.alive(), "interrupted job survived");
    host.send(b"printf '__%s__\\n' STILL_INTERACTIVE\n");
    host.until("shell survived Ctrl-C", |s| {
        s.contains("__STILL_INTERACTIVE__")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn sustained_output_does_not_starve_ctrl_c_or_the_next_command() {
    let mut host = Host::new();
    host.send(b"/usr/bin/yes FLOOD_MARKER\n");
    host.until("continuous output", |s| s.contains("FLOOD_MARKER"));
    host.send(b"\x03");
    host.send(b"printf '__%s__\\n' RECOVERED\n");
    host.until("shell after output flood", |s| s.contains("__RECOVERED__"));
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn host_resize_reaches_child_stty() {
    let mut host = Host::new();
    host.send(b"printf '__%s:' INITIAL; stty size\n");
    host.until("initial child dimensions", |s| {
        s.contains("__INITIAL:24 80")
    });
    host.master
        .resize(PtySize {
            rows: 31,
            cols: 101,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    host.terminal.resize(101, 31, 0, 0).unwrap();
    host.rows = 31;
    host.cols = 101;
    // Input and SIGWINCH can race. Let the child observe the new ioctl state,
    // with a bounded loop, rather than relying on a fixed sleep in the test.
    host.send(b"i=0; while [ \"$(stty size)\" != '31 101' ] && [ \"$i\" -lt 50 ]; do i=$((i+1)); sleep 0.1; done; printf '__%s:' RESIZED; stty size\n");
    host.until("resized child dimensions", |s| {
        s.contains("__RESIZED:31 101")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn idle_editor_resizes_without_keyboard_input() {
    check_idle_editor_resize("/usr/bin/vi -u NONE -N -n -i NONE");
}

#[test]
fn idle_neovim_resizes_without_keyboard_input() {
    if std::process::Command::new("nvim")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping Neovim smoke: nvim is unavailable");
        return;
    }
    check_idle_editor_resize("nvim --clean -n -i NONE");
}

fn check_idle_editor_resize(command: &str) {
    let mut host = Host::new();
    host.send(
        format!("{command} -c 'set laststatus=2 statusline=SIZE_%{{&columns}}x%{{&lines}}'\n")
            .as_bytes(),
    );
    host.until("initial editor dimensions", |s| s.contains("SIZE_80x24"));
    for (cols, rows) in [(101, 31), (47, 12), (80, 24)] {
        host.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        host.terminal.resize(cols, rows, 0, 0).unwrap();
        host.rows = rows;
        host.cols = cols;
        let expected = format!("SIZE_{cols}x{rows}");
        host.until("idle editor resize", |s| s.contains(&expected));
    }
    host.send(b":q!\r");
    host.until("shell after resized editor", |s| {
        s.contains("__SHELL:") && !s.contains("SIZE_80x24")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn in_band_resize_report_arrives_while_application_is_idle() {
    let mut host = Host::new();
    std::fs::write(
        host.home.join("resize_probe.py"),
        r#"import os, signal, termios, tty
saved = termios.tcgetattr(0)
def report():
    data = b''
    while not data.endswith(b't'):
        data += os.read(0, 1)
    return data
try:
    tty.setraw(0)
    signal.signal(signal.SIGWINCH, signal.SIG_IGN)
    os.write(1, b'\x1b[?2048h')
    os.write(1, b'\r\n__RESIZE_READY__\r\n')
    data = report()
    os.write(1, b'\r\n__REPORT:' + data.hex().encode() + b'__\r\n')
finally:
    os.write(1, b'\x1b[?2048l')
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#,
    )
    .unwrap();
    host.send(b"/usr/bin/python3 resize_probe.py\n");
    host.until("in-band resize enabled", |s| s.contains("__RESIZE_READY__"));
    host.master
        .resize(PtySize {
            rows: 31,
            cols: 101,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    host.terminal.resize(101, 31, 0, 0).unwrap();
    host.rows = 31;
    host.cols = 101;
    let expected = b"\x1b[48;31;101;0;0t"
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    host.until("in-band resize without subsequent output or input", |s| {
        s.contains(&format!("__REPORT:{expected}__"))
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn shell_exit_status_and_host_terminal_are_restored() {
    let mut host = Host::new();
    let raw = termios(&*host.master);
    assert_eq!(raw.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
    host.send(b"exit 23\n");
    host.wait_exit(23);
}

#[test]
fn external_sigterm_restores_host_and_kills_inner_session() {
    let mut host = Host::new();
    let shell = host.dependents[0];
    host.send(
        b"(trap '' HUP TERM; exec /bin/sleep 60) & printf '__%s:%s__\\n' BACKGROUND \"$!\"\n",
    );
    let job = host.track_pid("__BACKGROUND:");
    host.process.signal(libc::SIGTERM);
    host.wait_exit(0);
    let deadline = Instant::now() + Duration::from_secs(2);
    while (shell.alive() || job.alive()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!shell.alive(), "inner shell survived SIGTERM cleanup");
    assert!(!job.alive(), "background job survived SIGTERM cleanup");
}

#[test]
fn vi_alternate_screen_returns_to_shell_contents() {
    if !Path::new("/usr/bin/vi").exists() {
        eprintln!("skipping editor smoke: /usr/bin/vi is unavailable");
        return;
    }
    let mut host = Host::new();
    host.send(b"printf '__%s__\\n' BEFORE_EDITOR; /usr/bin/vi -u NONE -N -n -i NONE -c 'set laststatus=2 statusline=EDITOR_READY'\n");
    host.until("vi startup", |s| {
        s.contains("EDITOR_READY") && !s.contains("__BEFORE_EDITOR__")
    });
    host.send(b"iEDITOR_BUFFER_MARKER\x1b");
    host.until("vi buffer insertion", |s| {
        s.contains("EDITOR_BUFFER_MARKER")
    });
    host.send(b":q!\r");
    host.until("editor primary screen restored", |s| {
        s.contains("__BEFORE_EDITOR__") && !s.contains("EDITOR_BUFFER_MARKER")
    });
    host.send(b"printf '__%s__\\n' AFTER_EDITOR\n");
    host.until("shell after vi", |s| s.contains("__AFTER_EDITOR__"));
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn sgr_mouse_press_is_forwarded_to_tracking_application() {
    let mut host = Host::new();
    // dd/od is a deterministic application: enable SGR tracking, consume one
    // nine-byte press, print its hex bytes, then disable tracking. No release
    // or local selection is sent, so this never touches the real clipboard.
    host.send(b"stty raw -echo; printf '\\033[?1000h\\033[?1006h__%s__\\r\\n' MOUSE_READY; dd bs=1 count=9 2>/dev/null | od -An -tx1; printf '\\033[?1000l\\033[?1006l\\r\\n'; stty sane -echo; printf '__%s__\\n' MOUSE_DONE\n");
    host.until("mouse reader ready", |s| s.contains("__MOUSE_READY__"));
    host.send(b"\x1b[<0;4;3M");
    let screen = host.until("mouse reader complete", |s| s.contains("__MOUSE_DONE__"));
    let words: Vec<_> = screen.split_whitespace().collect();
    assert!(
        words
            .windows(9)
            .any(|bytes| bytes == ["1b", "5b", "3c", "30", "3b", "34", "3b", "33", "4d"]),
        "{screen}"
    );
    host.send(b"exit\n");
    host.wait_exit(0);
}
