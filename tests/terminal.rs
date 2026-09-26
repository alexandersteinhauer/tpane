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
// UI copy, deliberately independent of working directories and internal IDs.
const EXPLORER_TITLE: &str = "›";
const CLOSE_PROMPT: &str = "Close running programs?";

fn explorer_visible(screen: &str) -> bool {
    // PTY reads can split a frame; wait for the complete bottom border.
    screen.contains(EXPLORER_TITLE)
        && screen.lines().any(|line| {
            line.split_once('└').is_some_and(|(_, rest)| {
                rest.split_once('┘').is_some_and(|(border, _)| {
                    !border.is_empty() && border.chars().all(|c| c == '─')
                })
            })
        })
}

fn explorer_row(line: &str) -> Option<&str> {
    Some(line.split_once('│')?.1.split_once('│')?.0.trim())
}

fn selected_active_leaf(screen: &str) -> bool {
    screen.lines().any(|line| {
        line.split_once('›')
            .is_some_and(|(_, row)| row.contains(" ●"))
    })
}

// Snapshot the padded list and square-cornered border.
fn assert_explorer_snapshot(screen: &str, cols: u16, rows: u16, entries: &[&str]) {
    let width = usize::from(cols.min(78));
    let height = usize::from(rows).min((entries.len() + 4).clamp(5, 24));
    let left = (usize::from(cols) - width) / 2;
    let top = (usize::from(rows) - height) / 2;
    let actual: Vec<String> = screen
        .lines()
        .skip(top + 2)
        .take(height - 4)
        .map(|line| {
            line.chars()
                .skip(left + 2)
                .take(width - 4)
                .collect::<String>()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    let border = |row: usize| {
        screen
            .lines()
            .nth(row)
            .unwrap()
            .chars()
            .skip(left)
            .take(width)
            .collect::<String>()
    };
    assert_eq!(border(top), format!("┌{}┐", "─".repeat(width - 2)));
    assert_eq!(
        border(top + height - 1),
        format!("└{}┘", "─".repeat(width - 2))
    );
    assert_eq!(border(top + 1), format!("│{}│", " ".repeat(width - 2)));
    assert_eq!(
        border(top + height - 2),
        format!("│{}│", " ".repeat(width - 2))
    );
    assert_eq!(actual, entries, "{screen}");
    for hidden in ["Sessions", "All sessions", "↑↓ move", "esc back", "╭", "╯"] {
        assert!(!screen.contains(hidden), "unexpected chrome: {hidden}");
    }
}

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

    // Coordinates are zero-based host coordinates. Include the release so a
    // swallowed explorer press cannot leave mouse capture behind.
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

    fn explorer(&mut self) {
        self.send(b"\0");
        self.until("session explorer", explorer_visible);
    }

    fn dismissed(&mut self) {
        self.until("explorer dismissed", |s| {
            !s.contains(EXPLORER_TITLE) && !s.contains(CLOSE_PROMPT)
        });
    }

    fn add_sibling(&mut self) {
        self.explorer();
        self.send(b"n");
        self.dismissed();
    }

    fn add_child(&mut self) {
        self.explorer();
        self.send(b"N");
        self.dismissed();
    }

    // Address fixture nodes structurally, never by generated names. These
    // fixtures have a leaf first child and only leaves before the requested
    // sibling (Down therefore cannot enter an intervening subtree).
    // Home selects the first top-level session; nested groups remain navigable.
    fn explore_path(&mut self, path: &[usize]) {
        self.explorer();
        self.send(b"\x1b[H");
        for (depth, &index) in path.iter().enumerate() {
            if depth > 0 {
                self.send(b"\x1b[D\x1b[C\x1b[C");
            }
            for _ in 0..index {
                self.send(b"\x1b[B");
            }
        }
    }

    fn select_path(&mut self, path: &[usize]) {
        self.explore_path(path);
        self.send(b"\r");
        self.dismissed();
    }

    fn close_path(&mut self, path: &[usize]) {
        self.explore_path(path);
        self.send(b"d");
    }

    fn confirm_close(&mut self) {
        self.until("foreground close confirmation", |s| {
            s.contains(CLOSE_PROMPT)
        });
        self.send(b"\r");
        self.dismissed();
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

    // A quiet full-screen application records *every* SIGWINCH, including a
    // transient shrink/grow that stty before/after an explorer could miss.
    // SIGUSR1 asks for fresh output while the explorer owns keyboard input.
    fn frame_probe(&mut self) -> Process {
        std::fs::write(
            self.home.join("frame_probe.py"),
            r#"import os, signal, termios, tty
saved = termios.tcgetattr(0)
winches = 0
def resized(signum, frame):
    global winches
    winches += 1
def paint(label):
    cols, rows = os.get_terminal_size(0)
    data = b'\x1b[2J'
    for row in range(1, rows + 1):
        data += ('\x1b[%d;1H%s_%02d_' % (row, label, row)).encode() + b'.' * (cols - len(label) - 5)
    data += b'\x1b[1;1H__FRAME_PID:' + str(os.getpid()).encode() + b'__'
    data += ('\x1b[%d;1H__FRAME_%s__' % (rows, label)).encode()
    os.write(1, data)
try:
    tty.setraw(0)
    signal.signal(signal.SIGWINCH, resized)
    signal.signal(signal.SIGUSR1, lambda signum, frame: paint('UPDATE'))
    paint('BASE')
    data = bytearray()
    while not data.endswith(b'q'):
        data.extend(os.read(0, 1024))
    cols, rows = os.get_terminal_size(0)
    os.write(1, ('\x1b[2J\x1b[H__FRAME_DONE:%d:%d:%d:%s__\r\n' % (winches, rows, cols, data.hex())).encode())
finally:
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#,
        )
        .unwrap();
        self.send(b"/usr/bin/python3 frame_probe.py\n");
        let process = self.track_pid("__FRAME_PID:");
        self.until("complete static frame", |s| s.contains("__FRAME_BASE__"));
        process
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
    host.dimensions("RESIZED", 31);
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
fn kitty_double_option_opens_and_toggles_explorer() {
    let mut host = Host::new();
    // The fixture bootstrap uses legacy presses without releases. A real focus
    // boundary clears that key state before switching to Kitty event reporting.
    host.send(b"\x1b[O\x1b[I");
    let taps = b"\x1b[57443;3u\x1b[57443;1:3u\x1b[57443;3u\x1b[57443;1:3u";
    host.send(taps);
    host.until("double-Option explorer", explorer_visible);
    host.send(taps);
    host.dismissed();
    host.dimensions("DOUBLE_OPTION_SIZE", host.rows);
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn explorer_open_cancel_and_confirm_preserve_selection_and_full_viewport() {
    let mut host = Host::new();
    host.add_sibling();
    host.prepare_shell("SECOND");
    host.add_child();
    host.prepare_shell("FOURTH");
    let probe = host.frame_probe();
    let before = host.visible();
    assert!(before.starts_with("__FRAME_PID:"));
    assert!(before.lines().last().unwrap().contains("__FRAME_BASE__"));

    for opener in [b"\0".as_slice(), b"\x1b[32;5u\x1b[32;5:3u".as_slice()] {
        host.send(opener);
        let overlay = host.until("explorer initially selects the active leaf", |s| {
            explorer_visible(s) && selected_active_leaf(s)
        });
        assert_centered_overlay(&before, &overlay, host.cols, host.rows);
        assert_explorer_snapshot(
            &overlay,
            host.cols,
            host.rows,
            &["1", "▾ 2", "2.1", "› 2.2 ●"],
        );
        host.send(b"\x1b[A");
        host.until("Up changes the explorer cursor, not the active leaf", |s| {
            explorer_visible(s) && !selected_active_leaf(s)
        });
        host.send(b"\x1b[B");
        host.until("Down returns to the active leaf", selected_active_leaf);
        host.send(b"\x1b[A\x1b[A\x1b");
        host.until(
            "cancel restores every cell of the same nested session",
            |s| s == before,
        );
    }
    host.explorer();
    host.send(b"\r");
    host.until(
        "Enter on initially selected leaf restores the same session",
        |s| s == before,
    );
    host.send(b"q");
    host.until(
        "no transient resize or modal input reached application",
        |s| s.contains("__FRAME_DONE:0:24:80:71__"),
    );
    host.wait_dead(&[probe]);
    host.send(b"exit\n");
    host.until("nested sibling survives explorer cancel", |s| {
        s.contains("__SECOND:")
    });
    host.send(b"exit\n");
    host.until("first sibling survives explorer cancel", |s| {
        s.contains("__SHELL:")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

// Compare real terminal cells rather than repaint escape sequences. The changed
// rectangle must have balanced margins, without prescribing an exact dialog size.
fn assert_centered_overlay(
    before: &str,
    overlay: &str,
    cols: u16,
    rows: u16,
) -> (usize, usize, usize, usize) {
    let mut left = usize::from(cols);
    let mut top = usize::from(rows);
    let mut right = 0;
    let mut bottom = 0;
    for (y, (old, new)) in before.lines().zip(overlay.lines()).enumerate() {
        let old: Vec<_> = old
            .chars()
            .chain(std::iter::repeat(' '))
            .take(usize::from(cols))
            .collect();
        let new: Vec<_> = new
            .chars()
            .chain(std::iter::repeat(' '))
            .take(usize::from(cols))
            .collect();
        for x in 0..usize::from(cols) {
            if old[x] != new[x] {
                left = left.min(x);
                right = right.max(x);
                top = top.min(y);
                bottom = bottom.max(y);
            }
        }
    }
    assert!(
        left <= right && top <= bottom,
        "explorer did not change any cells"
    );
    assert!(
        left > 0 && top > 0 && right + 1 < usize::from(cols) && bottom + 1 < usize::from(rows),
        "overlay is not inset: {overlay}"
    );
    assert!(
        left.abs_diff(usize::from(cols) - right - 1) <= 1,
        "overlay is not horizontally centered: {overlay}"
    );
    assert!(
        top.abs_diff(usize::from(rows) - bottom - 1) <= 1,
        "overlay is not vertically centered: {overlay}"
    );
    (left, top, right, bottom)
}

#[test]
fn output_behind_explorer_does_not_overwrite_overlay_and_is_restored_on_dismiss() {
    let mut host = Host::new();
    let probe = host.frame_probe();
    let before = host.visible();
    host.explorer();
    let overlay = host.visible();
    assert_explorer_snapshot(&overlay, host.cols, host.rows, &["› 1 ●"]);
    let (left, top, right, bottom) =
        assert_centered_overlay(&before, &overlay, host.cols, host.rows);
    let rectangle = |screen: &str| -> Vec<String> {
        screen
            .lines()
            .skip(top)
            .take(bottom - top + 1)
            .map(|line| line.chars().skip(left).take(right - left + 1).collect())
            .collect()
    };
    let expected = rectangle(&overlay);
    probe.signal(libc::SIGUSR1);
    host.until("output drained and painted around intact explorer", |s| {
        s.contains("__FRAME_UPDATE__") && rectangle(s) == expected
    });
    host.send(b"\x1b");
    host.dismissed();
    host.until(
        "latest application frame restored under dismissed overlay",
        |s| {
            s.contains("__FRAME_UPDATE__")
                && s.lines().nth(12).unwrap_or("").starts_with("UPDATE_13_")
        },
    );
    assert!(!host.visible().contains("BASE_"));
    host.send(b"q");
    host.until("overlay output did not resize or inject input", |s| {
        s.contains("__FRAME_DONE:0:24:80:71__")
    });
    host.wait_dead(&[probe]);
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn explorer_consumes_unknown_commands_paste_mouse_and_key_releases() {
    let mut host = Host::new();
    std::fs::write(
        host.home.join("modal_input.py"),
        r#"import os, termios, tty
saved = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.write(1, b'\x1b[?1000h\x1b[?1006h\x1b[?2004h\r\n__MODAL_READY__\r\n')
    data = bytearray()
    while not data.endswith(b'AFTER\n'):
        data.extend(os.read(0, 4096))
    # A file avoids line wrapping hiding a long, wrongly forwarded payload.
    open('modal-input', 'wb').write(data)
    os.write(1, b'\r\n__MODAL_DONE__\r\n')
finally:
    os.write(1, b'\x1b[?1000l\x1b[?1006l\x1b[?2004l')
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#,
    )
    .unwrap();
    host.send(b"/usr/bin/python3 modal_input.py\n");
    host.until("mouse and bracketed-paste application ready", |s| {
        s.contains("__MODAL_READY__")
    });
    host.send(b"\x1b[32;5u\x1b[32;5:3u");
    host.until("kitty Ctrl-Space opens explorer", |s| {
        s.contains(EXPLORER_TITLE)
    });
    host.send(b"qxyz!\x1b[A\x1b[B\x1b[D\x1b[C");
    // Paste includes action keys, newline, and an opener; it must not create,
    // close, rename, switch, dismiss, or send anything to the foreground PTY.
    host.paste(b"printf '__PASTE_LEAK__'\nnNdr\r\x1b\0");
    host.click(0, 0);
    host.click(host.cols / 2, host.rows / 2);
    // Kitty Escape release follows dismissal and must not leak either.
    host.send(b"\x1b[27u\x1b[27;1:3u");
    host.dismissed();
    host.send(b"AFTER\n");
    host.until("application reads only post-dismissal input", |s| {
        s.contains("__MODAL_DONE__")
    });
    assert_eq!(
        std::fs::read(host.home.join("modal-input")).unwrap(),
        b"AFTER\n"
    );
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn nested_sibling_rename_and_group_close_are_completed_modal_actions() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.add_sibling();
    let second = host.prepare_shell("SECOND");
    host.add_child();
    let third = host.prepare_shell("THIRD");
    host.add_sibling();
    let fourth = host.prepare_shell("FOURTH");
    host.dimensions("NESTED_SIBLING_SIZE", 24);

    host.explorer();
    host.send(b"r");
    host.until("rename editor", |s| s.contains("Name:"));
    // Clear the default numbered path without depending on its length.
    host.send(&[127; 128]);
    host.send(b"renamed-session\r");
    host.dismissed();
    host.send(b"printf '__%s__\\n' RENAME_DISMISSED\n");
    host.until("rename completion returns input to the shell", |s| {
        s.contains("__RENAME_DISMISSED__")
    });
    host.explorer();
    host.until("renamed active leaf", |s| {
        s.lines().filter_map(explorer_row).any(|row| {
            row.starts_with('›') && row.contains("renamed-session") && row.ends_with(" ●")
        })
    });
    host.send(b"\x1b");
    host.dismissed();

    host.select_path(&[1, 0]);
    host.until("original wrapped leaf", |s| s.contains("__SECOND:"));
    host.select_path(&[1, 1]);
    host.until("original nested sibling", |s| s.contains("__THIRD:"));
    host.select_path(&[1, 2]);
    host.until("n added a sibling rather than another nesting level", |s| {
        s.contains("__FOURTH:")
    });
    // Close the nested group, then the remaining top-level session.
    host.close_path(&[1]);
    host.dismissed();
    host.close_path(&[0]);
    host.wait_exit(0);
    host.wait_dead(&[first, second, third, fourth]);
}

#[test]
fn first_sibling_preserves_shell_state_and_inherits_active_cwd() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.send(b"mkdir 'inherited directory'; cd 'inherited directory'; KEEP=original; printf '__%s__\\n' ORIGINAL_CONTENTS\n");
    host.until("original shell state", |s| {
        s.contains("__ORIGINAL_CONTENTS__")
    });

    host.add_sibling();
    let second = host.prepare_shell("SECOND");
    assert_ne!(first.pid, second.pid, "add did not select a new shell");
    assert!(!host.visible().contains("__ORIGINAL_CONTENTS__"));
    host.dimensions("SECOND_SIZE", 24);
    host.explorer();
    assert_explorer_snapshot(&host.visible(), host.cols, host.rows, &["1", "› 2 ●"]);
    host.send(b"\x1b");
    host.dismissed();
    host.send(b"printf '__%s:%s__\\n' CWD \"${PWD##*/}\"; printf '__%s:%s__\\n' NEW_STATE \"${KEEP-unset}\"\n");
    host.until("new shell inherits cwd, not shell variables", |s| {
        s.contains("__CWD:inherited directory__") && s.contains("__NEW_STATE:unset__")
    });

    host.select_path(&[0]);
    host.until("original contents restored", |s| {
        s.contains("__ORIGINAL_CONTENTS__") && !s.contains("__SECOND_SIZE:")
    });
    host.send(b"printf '__%s:%s:%s__\\n' ORIGINAL_STATE \"$KEEP\" \"$$\"\n");
    host.until("original shell and variables preserved", |s| {
        s.contains(&format!("__ORIGINAL_STATE:original:{}__", first.pid))
    });
    host.dimensions("FIRST_SIZE", 24);
    host.send(b"exit 7\n");
    host.until("remaining shell selected after exit", |s| {
        s.contains("__SECOND_SIZE:")
    });
    host.dimensions("REMAINING_SIZE", 24);
    host.send(b"exit 19\n");
    host.wait_exit(19);
}

#[test]
fn recursive_explorer_restores_descendants_and_prunes_only_empty_groups() {
    let mut host = Host::new();
    host.add_sibling();
    host.prepare_shell("SECOND");
    host.add_child();
    host.prepare_shell("FOURTH");
    host.dimensions("NESTED_SIZE", 24);

    host.select_path(&[0]);
    host.until("first ancestor branch", |s| {
        s.contains("__SHELL:") && !s.contains("__FOURTH:")
    });
    host.explorer();
    host.send(b"\x1b[A");
    host.until("Up from first leaf stays at the first session", |s| {
        explorer_visible(s)
            && s.lines()
                .filter_map(explorer_row)
                .any(|row| row.starts_with('›') && row.ends_with("1 ●"))
    });
    host.send(b"\r");
    host.dismissed();
    host.until("first session opens directly", |s| s.contains("__SHELL:"));
    host.select_path(&[1]);
    host.until("selected descendant restored", |s| s.contains("__FOURTH:"));
    host.select_path(&[1, 0]);
    host.until("other nested child selected", |s| {
        s.contains("__SECOND:") && !s.contains("__FOURTH:")
    });
    host.select_path(&[0]);
    host.until("first ancestor again", |s| s.contains("__SHELL:"));
    host.select_path(&[1]);
    host.until("changed descendant selection remembered", |s| {
        s.contains("__SECOND:") && !s.contains("__FOURTH:")
    });

    host.send(b"exit 3\n");
    host.until("remaining nested child restored", |s| {
        s.contains("__FOURTH:")
    });
    host.dimensions("SINGLE_NESTED_SIZE", 24);
    // Prove the one-child group remains in the explorer (not just that its
    // surviving terminal is reachable). Left first selects, then collapses it.
    host.explorer();
    host.send(b"\x1b[D");
    host.until("single-child group is the leaf's parent", |s| {
        explorer_visible(s)
            && s.lines()
                .filter_map(explorer_row)
                .any(|row| row.starts_with('›') && row.ends_with("▾ 2"))
    });
    host.send(b"\x1b[D");
    host.until("Left collapses the retained group", |s| {
        explorer_visible(s)
            && s.lines()
                .filter_map(explorer_row)
                .any(|row| row.starts_with('›') && row.ends_with("▸ 2"))
            && !s
                .lines()
                .filter_map(explorer_row)
                .any(|row| row.contains("2.1"))
    });
    host.send(b"\x1b");
    host.dismissed();
    host.select_path(&[0]);
    host.select_path(&[1, 0]);
    host.until("single-child nested group retained", |s| {
        s.contains("__FOURTH:")
    });
    host.send(b"exit 4\n");
    host.until("empty nested group pruned", |s| s.contains("__SHELL:"));
    host.dimensions("PRUNED_SIZE", 24);
    // Root remains selectable even after pruning its now-empty nested group.
    host.select_path(&[]);
    host.until("root selects surviving descendant", |s| {
        s.contains("__SHELL:")
    });
    host.send(b"exit 17\n");
    host.wait_exit(17);
}

#[test]
fn option_digits_select_nested_tabs_without_leaking_into_shells() {
    let mut host = Host::new();
    host.add_sibling();
    host.prepare_shell("SECOND");
    host.add_child();
    host.prepare_shell("FOURTH");

    // Kitty: Option down, 2 down/repeat/up, 1 down, Option up, 1 up.
    // The repeat must not select nested 2, and releases must not reach a child.
    host.send(
        b"\x1b[57443;3u\x1b[50;3u\x1b[50;3:2u\x1b[50;3:3u\x1b[49;3u\x1b[57443;1:3u\x1b[49;1:3u",
    );
    host.until("keyboard selects nested first tab", |s| {
        s.contains("__SECOND:") && !s.contains("__FOURTH:")
    });
    host.send(b"\x1b[57443;3u\x1b[49;3u\x1b[57443;1:3u\x1b[49;1:3u");
    host.until("Option release resets to root", |s| {
        s.contains("__SHELL:") && !s.contains("__SECOND:")
    });

    // Legacy hosts deliver ESC-prefixed Alt digits without release events.
    host.send(b"\x1b2\x1b2");
    host.until("legacy sequence selects nested second tab", |s| {
        s.contains("__FOURTH:")
    });
    host.send(b"printf '__%s__\\n' SHORTCUT_INPUT_CLEAN\n");
    host.until("shortcuts did not corrupt shell input", |s| {
        s.contains("__SHORTCUT_INPUT_CLEAN__")
    });
    host.send(b"exit\n");
    host.until("nested sibling after shortcut-selected exit", |s| {
        s.contains("__SECOND:")
    });
    host.send(b"exit\n");
    host.until("root sibling after nested exits", |s| {
        s.contains("__SHELL:")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn hidden_pane_drains_output_and_receives_in_band_resize() {
    let mut host = Host::new();
    host.add_sibling();
    host.prepare_shell("VISIBLE");
    host.select_path(&[0]);
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
    host.select_path(&[1]);
    host.until("probe hidden", |s| {
        s.contains("__VISIBLE:") && !s.contains("__HIDDEN_PID:")
    });
    host.resize(101, 31);
    host.dimensions("VISIBLE_RESIZED", 31);
    let done = host.home.join("hidden.done");
    let visible = host.until("hidden output drained without selecting pane", |_| {
        done.exists()
    });
    assert!(
        !visible.contains("__HIDDEN_DONE__"),
        "hidden output leaked into active pane"
    );
    host.select_path(&[0]);
    let report = b"\x1b[48;31;101;0;0t"
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    host.until("hidden output and resize report restored", |s| {
        s.contains("__HIDDEN_DONE__") && s.contains(&format!("__HIDDEN_REPORT:{report}__"))
    });
    host.wait_dead(&[probe]);
    host.dimensions("HIDDEN_RESIZED", 31);
    host.send(b"exit\n");
    host.until("visible shell after hidden shell exit", |s| {
        s.contains("__VISIBLE:")
    });
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
    host.add_sibling();
    let other = host.prepare_shell("INPUT_OTHER");
    host.resize(101, 31);
    host.dimensions("INPUT_OTHER_SIZE", 31);
    host.select_path(&[0]);
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
    host.dimensions("INPUT_READER_SIZE", 31);
    host.send(b"exit\n");
    host.until("other shell after reader exit", |s| {
        s.contains("__INPUT_OTHER:")
    });
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
    host.add_sibling();
    let second = host.prepare_shell("INPUT_SECOND");
    host.add_child();
    let fourth = host.prepare_shell("INPUT_FOURTH");
    let probe = host.blocked_input_probe();
    host.paste(&backpressure_payload());
    host.select_path(&[0]);
    host.until("unrelated pane selected during stalled input", |s| {
        s.contains("__SHELL:")
    });
    host.close_path(&[1]);
    host.until("full-input subtree close confirmation", |s| {
        s.contains(CLOSE_PROMPT)
    });
    assert!(second.alive() && fourth.alive() && probe.alive());
    host.confirm_close();
    host.until("survivor after full-input subtree close", |s| {
        s.contains("__SHELL:")
    });
    host.wait_dead(&[second, fourth, probe]);

    // Reuse the surviving group immediately, exercising active-tab teardown
    // as well as hidden-subtree teardown. Even 64 KiB exceeds the PTY buffer.
    for id in 0..2 {
        host.add_sibling();
        let shell = host.prepare_shell(&format!("REOPENED_{id}"));
        let probe = host.blocked_input_probe();
        host.paste(&vec![b'x'; 64 * 1024]);
        host.close_path(&[1]);
        host.until("reopened full-input tab close confirmation", |s| {
            s.contains(CLOSE_PROMPT)
        });
        assert!(shell.alive() && probe.alive());
        host.confirm_close();
        host.until("survivor after stalled writer close", |s| {
            s.contains("__SHELL:")
        });
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
    host.add_sibling();
    let second = host.prepare_shell("SECOND");
    host.add_child();
    let fourth = host.prepare_shell("FOURTH");
    host.send(b"/bin/sh -c 'printf \"__%s:%s__\\n\" FOREGROUND \"$$\"; exec /bin/sleep 60'\n");
    let job = host.track_pid("__FOREGROUND:");
    // Close a subtree while its busy descendant is hidden: checking only the
    // active shell would silently kill the job without asking.
    host.select_path(&[0]);
    host.until("unrelated shell selected", |s| s.contains("__SHELL:"));
    host.close_path(&[1]);
    host.until("foreground close confirmation", |s| {
        s.contains(CLOSE_PROMPT)
    });
    assert!(first.alive() && second.alive() && fourth.alive() && job.alive());
    host.send(b"\x1b");
    host.dismissed();
    host.send(b"printf '__%s__\\n' CANCEL_STILL_FIRST\n");
    host.until("cancel dismissed and preserved active shell", |s| {
        s.contains("__CANCEL_STILL_FIRST__")
    });
    host.select_path(&[1]);
    host.until("cancel preserved selected descendant contents", |s| {
        s.contains("__FOREGROUND:")
    });
    assert!(second.alive() && fourth.alive() && job.alive());
    host.close_path(&[1]);
    host.confirm_close();
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
fn background_job_does_not_prompt_and_is_cleaned_up_with_its_session() {
    let mut host = Host::new();
    let first = host.dependents[0];
    host.add_sibling();
    let second = host.prepare_shell("SECOND");
    host.send(
        b"(trap '' HUP TERM; exec /bin/sleep 60) & printf '__%s:%s__\\n' BACKGROUND \"$!\"\n",
    );
    let job = host.track_pid("__BACKGROUND:");
    host.close_path(&[1]);
    host.dismissed();
    host.until("background session closed without confirmation", |s| {
        s.contains("__SHELL:") && !s.contains(CLOSE_PROMPT)
    });
    host.wait_dead(&[second, job]);
    assert!(first.alive());
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn repeated_switches_redraw_clean_terminal_engines_without_stale_cells() {
    let mut host = Host::new();
    host.add_sibling();
    host.prepare_shell("SECOND");
    // No shell commands or other terminal output during the switching loop:
    // engine dirty flags alone cannot be used to decide whether to paint.
    host.send(
        b"printf '\\033[2J\\033[HSECOND_SCREEN\\033[8;40HSECOND_RIGHT\\033[24;1HSECOND_BOTTOM'\n",
    );
    let second = host.until("second static screen", |s| s.contains("SECOND_BOTTOM"));
    assert!(
        second.starts_with("SECOND_SCREEN"),
        "terminal origin is not host row zero"
    );
    assert_eq!(second.lines().last(), Some("SECOND_BOTTOM"));
    host.select_path(&[0]);
    host.until("first shell selected", |s| s.contains("__SHELL:"));
    host.send(b"printf '\\033[2J\\033[HFIRST_SCREEN\\033[4;2HFIRST_LEFT\\033[24;1HFIRST_BOTTOM'\n");
    let first = host.until("first static screen", |s| s.contains("FIRST_BOTTOM"));
    assert!(first.starts_with("FIRST_SCREEN"));
    assert_eq!(first.lines().last(), Some("FIRST_BOTTOM"));
    for _ in 0..8 {
        host.select_path(&[1]);
        host.until("clean second engine repainted exactly", |s| s == second);
        host.select_path(&[0]);
        host.until("clean first engine repainted exactly", |s| s == first);
    }
    host.send(b"exit\n");
    host.until("second screen after first shell exit", |s| s == second);
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn nested_terminal_mouse_uses_unmodified_host_coordinates() {
    let mut host = Host::new();
    host.add_sibling();
    host.prepare_shell("SECOND");
    host.add_child();
    host.prepare_shell("FOURTH");
    host.dimensions("NESTED_MOUSE_SIZE", 24);
    host.send(b"stty raw -echo; printf '\\033[?1000h\\033[?1006h__%s__\\r\\n' MOUSE_READY; dd bs=1 count=9 2>/dev/null | od -An -tx1; printf '\\033[?1000l\\033[?1006l\\r\\n'; stty sane -echo; printf '__%s__\\n' MOUSE_DONE\n");
    host.until("nested mouse reader ready", |s| {
        s.contains("__MOUSE_READY__")
    });
    host.explorer();
    host.click(3, 2);
    host.send(b"\x1b");
    host.dismissed();
    // Nesting does not offset mouse coordinates: host row 5 is child row 5.
    host.send(b"\x1b[<0;4;5M");
    let screen = host.until("nested mouse reader complete", |s| {
        s.contains("__MOUSE_DONE__")
    });
    assert!(
        screen
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(9)
            .any(|bytes| bytes == ["1b", "5b", "3c", "30", "3b", "34", "3b", "35", "4d"]),
        "{screen}"
    );
    host.send(b"exit\n");
    host.until("nested sibling after mouse reader", |s| {
        s.contains("__SECOND:")
    });
    host.send(b"exit\n");
    host.until("root sibling after mouse reader", |s| {
        s.contains("__SHELL:")
    });
    host.send(b"exit\n");
    host.wait_exit(0);
}

#[test]
fn sgr_mouse_press_is_forwarded_to_tracking_application() {
    let mut host = Host::new();
    // dd/od is a deterministic application: enable SGR tracking, consume one
    // nine-byte press, print its hex bytes, then disable tracking. No terminal
    // selection touches the real clipboard.
    host.send(b"stty raw -echo; printf '\\033[?1000h\\033[?1006h__%s__\\r\\n' MOUSE_READY; dd bs=1 count=9 2>/dev/null | od -An -tx1; printf '\\033[?1000l\\033[?1006l\\r\\n'; stty sane -echo; printf '__%s__\\n' MOUSE_DONE\n");
    host.until("mouse reader ready", |s| s.contains("__MOUSE_READY__"));
    // The top row is terminal content, even in a single-session tree.
    host.send(b"\x1b[<0;4;1M");
    let screen = host.until("mouse reader complete", |s| s.contains("__MOUSE_DONE__"));
    let words: Vec<_> = screen.split_whitespace().collect();
    assert!(
        words
            .windows(9)
            .any(|bytes| bytes == ["1b", "5b", "3c", "30", "3b", "34", "3b", "31", "4d"]),
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
