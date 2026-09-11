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
        let pair = {
            static ALLOCATION: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _guard = ALLOCATION
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            native_pty_system()
                .openpty(PtySize {
                    rows: 24,
                    cols: 80,
                    pixel_width: 0,
                    pixel_height: 0,
                })
                .unwrap()
        };
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
        let mut command = CommandBuilder::new(
            std::env::var_os("TPANE_TEST_BINARY")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_tpane").into()),
        );
        command.args(["--shell", "/bin/sh"]);
        command.cwd(&home);
        command.env("HOME", &home);
        command.env("TPANE_TRACE_FILE", home.join("trace.log"));
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

    // Chrome interactions always include a release, so mouse capture cannot
    // leak into subsequent input. Coordinates in these helpers are zero-based.
    fn click(&mut self, col: u16, row: u16) {
        self.send(
            format!(
                "\x1b[<0;{};{}M\x1b[<0;{};{}m",
                col + 1,
                row + 1,
                col + 1,
                row + 1
            )
            .as_bytes(),
        );
    }

    fn plus(&mut self, row: u16) {
        self.click(self.cols - 2, row);
    }

    fn select(&mut self, row: u16, index: u16, count: u16) {
        self.click((self.cols - 3) * index / count + 1, row);
    }

    fn close_tab(&mut self, row: u16, index: u16, count: u16) {
        self.click((self.cols - 3) * (index + 1) / count - 2, row);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        self.terminal.resize(cols, rows, 0, 0).unwrap();
        self.rows = rows;
        self.cols = cols;
    }

    fn prepare_shell(&mut self, marker: &str) -> Process {
        self.send(
            format!("stty -echo; set +H; PS1=''; PS2=''; printf '__%s:%s__\\n' {marker} \"$$\"\n")
                .as_bytes(),
        );
        self.track_pid(&format!("__{marker}:"))
    }

    // A real foreground process that cannot drain its PTY until the test
    // explicitly releases it. Block SIGUSR1 before READY so sigwait also
    // consumes a release delivered before it starts waiting (no pause race).
    fn blocked_input_probe(&mut self) -> Process {
        std::fs::write(
            self.home.join("blocked_input.py"),
            r#"import hashlib, os, signal, termios, tty
signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGUSR1})
saved = termios.tcgetattr(0)
def read_exact(count):
    data = bytearray()
    while len(data) < count:
        chunk = os.read(0, min(65536, count - len(data)))
        if not chunk:
            raise RuntimeError('premature EOF')
        data.extend(chunk)
    return data
try:
    tty.setraw(0)
    # Host paste delimiters must not become part of the child's payload.
    os.write(1, b'\x1b[?2004l\r\n__INPUT_READY:' + str(os.getpid()).encode() + b'__\r\n')
    signal.sigwait({signal.SIGUSR1})
    payload = read_exact(1024 * 1024)
    # A separately queued trailer catches extra bytes after the payload too.
    assert read_exact(12) == b'END_OF_PASTE', 'duplicated or reordered input'
    os.write(1, b'\r\n__SHA256:' + hashlib.sha256(payload).hexdigest().encode() + b'__\r\n')
finally:
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#,
        )
        .unwrap();
        self.send(b"/usr/bin/python3 blocked_input.py\n");
        self.track_pid("__INPUT_READY:")
    }

    fn paste(&mut self, payload: &[u8]) {
        self.send(b"\x1b[200~");
        self.send(payload);
        self.send(b"\x1b[201~");
    }

    fn dimensions(&mut self, marker: &str, rows: u16) {
        // Host input and SIGWINCH are independent streams. Wait in the child
        // for the ioctl size instead of assuming the resize won that race.
        self.send(format!("i=0; while [ \"$(stty size)\" != '{rows} {}' ] && [ \"$i\" -lt 50 ]; do i=$((i+1)); sleep 0.1; done; printf '__%s:' {marker}; stty size\n", self.cols).as_bytes());
        let expected = format!("__{marker}:{rows} {}", self.cols);
        self.until("pane viewport dimensions", |s| s.contains(&expected));
    }

    fn bars(&mut self, labels: &[&[&str]]) {
        // Match entire rows: catches stale labels/close controls after a tree
        // mutation, incorrect rounding, and terminal output overwriting chrome.
        let expected: Vec<_> = labels
            .iter()
            .map(|labels| {
                let available = usize::from(self.cols - 3);
                let mut row = String::new();
                for (i, label) in labels.iter().enumerate() {
                    let width = available * (i + 1) / labels.len() - available * i / labels.len();
                    let close = if width >= 6 { "[x]" } else { "" };
                    let text_width = width - close.len();
                    let text: String = format!(" {label}").chars().take(text_width).collect();
                    row.push_str(&format!("{text:<text_width$}{close}"));
                }
                row.push_str(&" ".repeat(available - row.len()));
                row.push_str("[+]");
                row
            })
            .collect();
        self.until("exact tab bars", |s| {
            s.lines()
                .take(expected.len())
                .eq(expected.iter().map(String::as_str))
        });
    }

    fn wait_dead(&mut self, processes: &[Process]) {
        self.until("closed pane processes to terminate", |_| {
            processes.iter().all(|p| !p.alive())
        });
    }

    fn send(&mut self, bytes: &[u8]) {
        let deadline = Instant::now() + TIMEOUT;
        let mut remaining = bytes;
        while !remaining.is_empty() {
            match self.writer.write(remaining) {
                Ok(0) => panic!("PTY input closed"),
                Ok(n) => remaining = &remaining[n..],
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // A large paste needs many outer-PTY writes. Waiting only
                    // for output imposes pump's 20 ms timeout on every short
                    // write when the application is (correctly) silent.
                    let mut poll = libc::pollfd {
                        fd: self.master.as_raw_fd().unwrap(),
                        events: libc::POLLIN | libc::POLLOUT,
                        revents: 0,
                    };
                    let ready = unsafe { libc::poll(&mut poll, 1, 20) };
                    if ready < 0 {
                        assert_eq!(
                            io::Error::last_os_error().kind(),
                            io::ErrorKind::Interrupted
                        );
                    } else if poll.revents & libc::POLLIN != 0 {
                        self.pump();
                    }
                }
                Err(e) => panic!("PTY input: {e}"),
            }
            assert!(
                Instant::now() < deadline,
                "PTY input timed out with {} of {} bytes remaining",
                remaining.len(),
                bytes.len()
            );
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
        s.contains("__INITIAL:23 80")
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
    host.send(b"i=0; while [ \"$(stty size)\" != '30 101' ] && [ \"$i\" -lt 50 ]; do i=$((i+1)); sleep 0.1; done; printf '__%s:' RESIZED; stty size\n");
    host.until("resized child dimensions", |s| {
        s.contains("__RESIZED:30 101")
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
    host.until("initial editor dimensions", |s| s.contains("SIZE_80x23"));
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
        let expected = format!("SIZE_{cols}x{}", rows - 1);
        host.until("idle editor resize", |s| s.contains(&expected));
    }
    host.send(b":q!\r");
    host.until("shell after resized editor", |s| {
        s.contains("__SHELL:") && !s.contains("SIZE_80x23")
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
    let expected = b"\x1b[48;30;101;0;0t"
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
fn first_plus_preserves_shell_state_and_inherits_active_cwd() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.bars(&[&[]]);
    host.send(b"mkdir 'inherited directory'; cd 'inherited directory'; KEEP=original; printf '__%s__\\n' ORIGINAL_CONTENTS\n");
    host.until("original shell state", |s| {
        s.contains("__ORIGINAL_CONTENTS__")
    });

    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    let second = host.prepare_shell("SECOND");
    assert_ne!(first.pid, second.pid, "plus did not select a new shell");
    assert!(!host.visible().contains("__ORIGINAL_CONTENTS__"));
    host.dimensions("SECOND_SIZE", 22);
    host.send(b"printf '__%s:%s__\\n' CWD \"${PWD##*/}\"; printf '__%s:%s__\\n' NEW_STATE \"${KEEP-unset}\"\n");
    host.until("new shell inherits cwd, not shell variables", |s| {
        s.contains("__CWD:inherited directory__") && s.contains("__NEW_STATE:unset__")
    });

    host.select(0, 0, 2);
    host.until("original contents restored", |s| {
        s.contains("__ORIGINAL_CONTENTS__") && !s.contains("__SECOND_SIZE:")
    });
    host.send(b"printf '__%s:%s:%s__\\n' ORIGINAL_STATE \"$KEEP\" \"$$\"\n");
    host.until("original shell and variables preserved", |s| {
        s.contains(&format!("__ORIGINAL_STATE:original:{}__", first.pid))
    });
    host.dimensions("FIRST_SIZE", 22);
    host.send(b"exit 7\n");
    // Groups with one child are not collapsed; the terminal still has two bars.
    host.bars(&[&["Shell 2"], &[]]);
    host.until("remaining shell selected after exit", |s| {
        s.contains("__SECOND_SIZE:")
    });
    host.dimensions("REMAINING_SIZE", 22);
    host.send(b"exit 19\n");
    host.wait_exit(19);
}

#[test]
fn recursive_tabs_restore_selected_descendants_and_prune_only_empty_groups() {
    let mut host = Host::new();
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    host.prepare_shell("SECOND");
    host.plus(1);
    host.bars(&[&["Shell 1", "Tabs 5"], &["Shell 2", "Shell 4"], &[]]);
    host.prepare_shell("FOURTH");
    host.dimensions("NESTED_SIZE", 21);

    host.select(0, 0, 2);
    host.until("first ancestor branch", |s| {
        s.contains("__SHELL:") && !s.contains("__FOURTH:")
    });
    host.bars(&[&["Shell 1", "Tabs 5"], &[]]);
    host.select(0, 1, 2);
    host.until("selected descendant restored", |s| s.contains("__FOURTH:"));
    host.select(1, 0, 2);
    host.until("other nested child selected", |s| {
        s.contains("__SECOND:") && !s.contains("__FOURTH:")
    });
    host.select(0, 0, 2);
    host.until("first ancestor again", |s| s.contains("__SHELL:"));
    host.select(0, 1, 2);
    host.until("changed descendant selection remembered", |s| {
        s.contains("__SECOND:") && !s.contains("__FOURTH:")
    });

    host.send(b"exit 3\n");
    host.bars(&[&["Shell 1", "Tabs 5"], &["Shell 4"], &[]]);
    host.until("single-child nested group retained", |s| {
        s.contains("__FOURTH:")
    });
    host.dimensions("SINGLE_NESTED_SIZE", 21);
    host.send(b"exit 4\n");
    host.bars(&[&["Shell 1"], &[]]);
    host.until("empty nested group pruned", |s| s.contains("__SHELL:"));
    host.dimensions("PRUNED_SIZE", 22);
    host.send(b"exit 17\n");
    host.wait_exit(17);
}

#[test]
fn hidden_pane_drains_output_and_receives_in_band_resize() {
    let mut host = Host::new();
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    host.prepare_shell("VISIBLE");
    host.select(0, 0, 2);
    host.until("first shell selected", |s| s.contains("__SHELL:"));
    std::fs::write(
        host.home.join("hidden_probe.py"),
        r#"import os, signal, termios, tty
saved = termios.tcgetattr(0)
try:
    tty.setraw(0)
    signal.signal(signal.SIGWINCH, signal.SIG_IGN)
    os.write(1, b'\x1b[?2048h\r\n__HIDDEN_PID:' + str(os.getpid()).encode() + b'__\r\n')
    data = b''
    while not data.endswith(b't'):
        data += os.read(0, 1)
    # Far more than a PTY buffer: completion proves hidden output was drained.
    for i in range(8192):
        os.write(1, b'HIDDEN_OUTPUT_PADDING_0123456789\r\n')
    os.write(1, b'__HIDDEN_REPORT:' + data.hex().encode() + b'__\r\n__HIDDEN_DONE__\r\n')
finally:
    os.write(1, b'\x1b[?2048l')
    termios.tcsetattr(0, termios.TCSANOW, saved)
open('hidden.done', 'w').close()
"#,
    )
    .unwrap();
    host.send(b"/usr/bin/python3 hidden_probe.py\n");
    let probe = host.track_pid("__HIDDEN_PID:");
    host.select(0, 1, 2);
    host.until("probe hidden", |s| {
        s.contains("__VISIBLE:") && !s.contains("__HIDDEN_PID:")
    });
    host.resize(101, 31);
    host.dimensions("VISIBLE_RESIZED", 29);
    let done = host.home.join("hidden.done");
    let visible = host.until("hidden output drained without selecting pane", |_| {
        done.exists()
    });
    assert!(
        !visible.contains("__HIDDEN_DONE__"),
        "hidden output leaked into active pane"
    );
    host.select(0, 0, 2);
    let report = b"\x1b[48;29;101;0;0t"
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    host.until("hidden output and resize report restored", |s| {
        s.contains("__HIDDEN_DONE__") && s.contains(&format!("__HIDDEN_REPORT:{report}__"))
    });
    host.wait_dead(&[probe]);
    host.dimensions("HIDDEN_RESIZED", 29);
    host.send(b"exit\n");
    host.bars(&[&["Shell 2"], &[]]);
    host.send(b"exit\n");
    host.wait_exit(0);
}

// Every 16-byte record is distinct, so duplicating/reordering whole PTY write
// chunks cannot accidentally preserve the checksum of a short repeating pattern.
fn backpressure_payload() -> Vec<u8> {
    (0..65536)
        .map(|i| format!("{i:015x};"))
        .collect::<String>()
        .into_bytes()
}

#[test]
fn full_pty_input_keeps_ui_responsive_and_delivers_every_byte_in_order() {
    let mut host = Host::new();
    let probe = host.blocked_input_probe();
    let payload = backpressure_payload();
    assert_eq!(payload.len(), 1024 * 1024);
    // Much larger than the kernel PTY input buffer, with no reader running.
    host.paste(&payload);
    host.paste(b"END_OF_PASTE");

    // These events follow the complete paste in the host input stream. Their
    // acknowledgements prove the UI is servicing input while the write stalls.
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    let other = host.prepare_shell("INPUT_OTHER");
    host.resize(101, 31);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    host.dimensions("INPUT_OTHER_SIZE", 29);
    host.select(0, 0, 2);
    host.until("blocked reader selected again", |s| {
        s.contains("__INPUT_READY:")
    });
    assert!(probe.alive(), "blocked reader exited before release");
    assert!(!host.visible().contains("__SHA256:"));

    probe.signal(libc::SIGUSR1);
    // SHA-256 of the numbered ASCII records above, computed independently.
    host.until("complete ordered paste and trailer", |s| {
        s.contains("__SHA256:b9de5284ccf4147337090f12a132c24723c70a9e00bb028f29f18173805e6664__")
    });
    host.wait_dead(&[probe]);
    host.dimensions("INPUT_READER_SIZE", 29);
    host.send(b"exit\n");
    host.bars(&[&["Shell 2"], &[]]);
    assert!(other.alive());
    host.send(b"printf '__%s__\\n' AFTER_INPUT_DRAIN\n");
    host.until("other pane usable after drain", |s| {
        s.contains("__AFTER_INPUT_DRAIN__")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
    host.wait_dead(&[host.dependents[0], other]);
}

#[test]
fn full_pty_input_subtree_close_and_reopen_leave_no_children() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    let second = host.prepare_shell("INPUT_SECOND");
    host.plus(1);
    host.bars(&[&["Shell 1", "Tabs 5"], &["Shell 2", "Shell 4"], &[]]);
    let fourth = host.prepare_shell("INPUT_FOURTH");
    let probe = host.blocked_input_probe();
    host.paste(&backpressure_payload());
    host.select(0, 0, 2);
    host.until("unrelated pane selected during stalled input", |s| {
        s.contains("__SHELL:")
    });
    host.close_tab(0, 1, 2);
    host.until("full-input subtree close confirmation", |s| {
        s.contains("[Cancel] [Close]")
    });
    assert!(second.alive() && fourth.alive() && probe.alive());
    host.click(host.cols - 4, 0);
    host.bars(&[&["Shell 1"], &[]]);
    host.wait_dead(&[second, fourth, probe]);

    // Reuse the surviving group immediately, exercising active-tab teardown
    // as well as hidden-subtree teardown. Even 64 KiB exceeds the PTY buffer.
    for id in [6, 8] {
        host.plus(0);
        host.bars(&[&["Shell 1", &format!("Shell {id}")], &[]]);
        let shell = host.prepare_shell(&format!("REOPENED_{id}"));
        let probe = host.blocked_input_probe();
        host.paste(&vec![b'x'; 64 * 1024]);
        host.close_tab(0, 1, 2);
        host.until("reopened full-input tab close confirmation", |s| {
            s.contains("[Cancel] [Close]")
        });
        assert!(shell.alive() && probe.alive());
        host.click(host.cols - 4, 0);
        host.bars(&[&["Shell 1"], &[]]);
        host.wait_dead(&[shell, probe]);
        assert!(
            first.alive(),
            "closing stalled writer killed unrelated shell"
        );
        host.send(format!("printf '__%s__\\n' SURVIVED_INPUT_CLOSE_{id}\n").as_bytes());
        host.until("surviving pane usable after stalled writer close", |s| {
            s.contains(&format!("__SURVIVED_INPUT_CLOSE_{id}__"))
        });
    }
    host.send(b"exit\n");
    host.wait_exit(0);
    host.wait_dead(&[first]);
}

#[test]
fn subtree_close_confirms_foreground_job_and_cancel_preserves_every_session() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    let second = host.prepare_shell("SECOND");
    host.plus(1);
    host.bars(&[&["Shell 1", "Tabs 5"], &["Shell 2", "Shell 4"], &[]]);
    let fourth = host.prepare_shell("FOURTH");
    host.send(b"/bin/sh -c 'printf \"__%s:%s__\\n\" FOREGROUND \"$$\"; exec /bin/sleep 60'\n");
    let job = host.track_pid("__FOREGROUND:");
    // Close a subtree while its busy descendant is hidden: checking only the
    // active shell would silently kill the job without asking.
    host.select(0, 0, 2);
    host.until("unrelated shell selected", |s| s.contains("__SHELL:"));
    host.close_tab(0, 1, 2);
    host.until("foreground close confirmation", |s| {
        s.lines().next().unwrap_or("").ends_with("[Cancel] [Close]")
    });
    assert!(first.alive() && second.alive() && fourth.alive() && job.alive());
    host.click(host.cols - 15, 0);
    host.bars(&[&["Shell 1", "Tabs 5"], &[]]);
    host.select(0, 1, 2);
    host.until("cancel preserved selected descendant contents", |s| {
        s.contains("__FOREGROUND:")
    });
    assert!(second.alive() && fourth.alive() && job.alive());
    host.close_tab(0, 1, 2);
    host.until("second close confirmation", |s| {
        s.contains("[Cancel] [Close]")
    });
    host.click(host.cols - 4, 0);
    host.bars(&[&["Shell 1"], &[]]);
    host.until("surviving root child restored", |s| {
        s.contains("__SHELL:") && !s.contains("__FOREGROUND:")
    });
    host.wait_dead(&[second, fourth, job]);
    assert!(first.alive(), "subtree close killed unrelated shell");
    host.send(b"printf '__%s__\\n' SURVIVED_CLOSE\n");
    host.until("surviving shell usable", |s| {
        s.contains("__SURVIVED_CLOSE__")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn background_job_does_not_prompt_and_is_cleaned_up_with_its_tab() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    let second = host.prepare_shell("SECOND");
    host.send(
        b"(trap '' HUP TERM; exec /bin/sleep 60) & printf '__%s:%s__\\n' BACKGROUND \"$!\"\n",
    );
    let job = host.track_pid("__BACKGROUND:");
    host.close_tab(0, 1, 2);
    host.bars(&[&["Shell 1"], &[]]);
    host.until("background tab closed without confirmation", |s| {
        s.contains("__SHELL:") && !s.contains("[Cancel]")
    });
    host.wait_dead(&[second, job]);
    assert!(first.alive());
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn repeated_switches_redraw_clean_terminal_engines_without_stale_cells() {
    let mut host = Host::new();
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    host.prepare_shell("SECOND");
    // No shell commands or other terminal output during the switching loop:
    // engine dirty flags alone cannot be used to decide whether to paint.
    host.send(
        b"printf '\\033[2J\\033[HSECOND_SCREEN\\033[8;40HSECOND_RIGHT\\033[22;1HSECOND_BOTTOM'\n",
    );
    let second = host.until("second static screen", |s| s.contains("SECOND_BOTTOM"));
    let second_body = second.lines().skip(2).collect::<Vec<_>>().join("\n");
    host.select(0, 0, 2);
    host.until("first shell selected", |s| s.contains("__SHELL:"));
    host.send(b"printf '\\033[2J\\033[HFIRST_SCREEN\\033[4;2HFIRST_LEFT\\033[22;1HFIRST_BOTTOM'\n");
    let first = host.until("first static screen", |s| s.contains("FIRST_BOTTOM"));
    let first_body = first.lines().skip(2).collect::<Vec<_>>().join("\n");
    for _ in 0..8 {
        host.select(0, 1, 2);
        host.until("clean second engine repainted exactly", |s| {
            s.lines().skip(2).collect::<Vec<_>>().join("\n") == second_body
        });
        host.bars(&[&["Shell 1", "Shell 2"], &[]]);
        host.select(0, 0, 2);
        host.until("clean first engine repainted exactly", |s| {
            s.lines().skip(2).collect::<Vec<_>>().join("\n") == first_body
        });
    }
    host.send(b"exit\n");
    host.bars(&[&["Shell 2"], &[]]);
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn nested_chrome_is_consumed_and_terminal_mouse_uses_leaf_coordinates() {
    let mut host = Host::new();
    host.plus(0);
    host.bars(&[&["Shell 1", "Shell 2"], &[]]);
    host.prepare_shell("SECOND");
    host.plus(1);
    host.bars(&[&["Shell 1", "Tabs 5"], &["Shell 2", "Shell 4"], &[]]);
    host.prepare_shell("FOURTH");
    host.send(b"stty raw -echo; printf '\\033[?1000h\\033[?1006h__%s__\\r\\n' MOUSE_READY; dd bs=1 count=9 2>/dev/null | od -An -tx1; printf '\\033[?1000l\\033[?1006l\\r\\n'; stty sane -echo; printf '__%s__\\n' MOUSE_DONE\n");
    host.until("nested mouse reader ready", |s| {
        s.contains("__MOUSE_READY__")
    });
    host.select(0, 1, 2);
    host.select(1, 1, 2);
    host.click(3, 2);
    // Host row 5 is leaf row 2 after subtracting all three active bars.
    host.send(b"\x1b[<0;4;5M");
    let screen = host.until("nested mouse reader complete", |s| {
        s.contains("__MOUSE_DONE__")
    });
    assert!(
        screen
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(9)
            .any(|bytes| bytes == ["1b", "5b", "3c", "30", "3b", "34", "3b", "32", "4d"]),
        "{screen}"
    );
    host.send(b"exit\n");
    host.bars(&[&["Shell 1", "Tabs 5"], &["Shell 2"], &[]]);
    host.send(b"exit\n");
    host.bars(&[&["Shell 1"], &[]]);
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn sgr_mouse_press_is_forwarded_to_tracking_application() {
    let mut host = Host::new();
    // dd/od is a deterministic application: enable SGR tracking, consume one
    // nine-byte press, print its hex bytes, then disable tracking. Only chrome
    // receives a release; no terminal selection touches the real clipboard.
    host.send(b"stty raw -echo; printf '\\033[?1000h\\033[?1006h__%s__\\r\\n' MOUSE_READY; dd bs=1 count=9 2>/dev/null | od -An -tx1; printf '\\033[?1000l\\033[?1006l\\r\\n'; stty sane -echo; printf '__%s__\\n' MOUSE_DONE\n");
    host.until("mouse reader ready", |s| s.contains("__MOUSE_READY__"));
    // The empty leaf bar is chrome even when the child requests mouse input.
    // Its press AND release must be consumed, not counted by dd.
    host.click(3, 0);
    host.send(b"\x1b[<0;4;3M");
    let screen = host.until("mouse reader complete", |s| s.contains("__MOUSE_DONE__"));
    let words: Vec<_> = screen.split_whitespace().collect();
    assert!(
        words
            .windows(9)
            .any(|bytes| bytes == ["1b", "5b", "3c", "30", "3b", "34", "3b", "32", "4d"]),
        "{screen}"
    );
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn slow_host_output_does_not_block_input_or_bounded_shutdown() {
    let mut host = Host::new();
    std::fs::write(
        host.home.join("host_pressure.py"),
        r#"import os, select, termios, tty
saved = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.set_blocking(1, False)
    os.write(1, b'\r\n__PRESSURE_PID:' + str(os.getpid()).encode() + b'__\r\n')
    flooding = False
    while True:
        ready, writable, _ = select.select([0], [1] if flooding else [], [])
        if ready:
            data = os.read(0, 1024)
            if not data: break
            if b'f' in data: flooding = True
            if b'r' in data: open('input-received', 'w').close()
        if writable:
            try: os.write(1, b'HOST_OUTPUT_PRESSURE_0123456789\r\n' * 512)
            except BlockingIOError: pass
finally:
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#,
    )
    .unwrap();
    host.send(b"/usr/bin/python3 host_pressure.py\n");
    let producer = host.track_pid("__PRESSURE_PID:");
    host.send(b"f");
    host.master
        .resize(PtySize {
            rows: 100,
            cols: 200,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    // Deliberately stop reading the host PTY, including while sending input.
    std::thread::sleep(Duration::from_millis(300));
    host.writer.write_all(b"r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !host.home.join("input-received").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        host.home.join("input-received").exists(),
        "blocked host output prevented input dispatch"
    );
    host.process.signal(libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !host.exited && Instant::now() < deadline {
        host.exited = host.child.try_wait().unwrap().is_some();
        std::thread::sleep(Duration::from_millis(5));
    }
    if !host.exited {
        eprintln!(
            "trace: {:?}",
            std::fs::read_to_string(host.home.join("trace.log"))
        );
        let _ = std::process::Command::new("/bin/ps")
            .args([
                "-p",
                host.process.pid.to_string().as_str(),
                "-o",
                "pid,ppid,stat,wchan,command",
            ])
            .status();
    }
    assert!(
        host.exited,
        "shutdown waited indefinitely for an unread host"
    );
    assert!(!producer.alive(), "shutdown left the producer alive");
    let after = termios(&*host.master);
    let mask = libc::ICANON | libc::ECHO | libc::ISIG;
    assert_eq!(after.c_lflag & mask, host.before.c_lflag & mask);
    // Visual reset cannot be guaranteed when the receiver refuses all output.
}
