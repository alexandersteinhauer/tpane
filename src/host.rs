use std::{
    io::{self, IsTerminal},
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Result, ensure};
use crossterm::{
    event::{
        EnableBracketedPaste, EnableFocusChange, EnableMouseCapture, KeyboardEnhancementFlags,
        PushKeyboardEnhancementFlags,
    },
    execute,
    terminal::{self, EnterAlternateScreen},
};

static ACTIVE: AtomicBool = AtomicBool::new(false);

pub const RESET: &[u8] = b"\x1b[?2026l\x1b[<u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\x1b[0m\x1b[0 q\x1b[?7h\x1b[?25h\x1b[?1049l";

pub struct Host;

impl Host {
    pub fn enter() -> Result<Self> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "tpane requires an interactive terminal on stdin and stdout"
        );
        terminal::enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        std::panic::set_hook(Box::new(|info| {
            emergency_restore();
            best_effort(
                libc::STDERR_FILENO,
                format!("tpane panic: {info}\r\n").as_bytes(),
            );
        }));
        Ok(Self)
    }

    pub fn finish(&mut self) -> Result<()> {
        ACTIVE.store(false, Ordering::SeqCst);
        terminal::disable_raw_mode()?;
        Ok(())
    }
}

pub fn setup() -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    execute!(
        bytes,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste,
        EnableFocusChange,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        )
    )?;
    bytes.extend_from_slice(b"\x1b[?7l\x1b[2J\x1b[H");
    Ok(bytes)
}

impl Drop for Host {
    fn drop(&mut self) {
        emergency_restore();
    }
}

pub fn best_effort(fd: libc::c_int, bytes: &[u8]) {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return;
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return;
    }
    let mut offset = 0;
    while offset < bytes.len() {
        let written =
            unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
        if written > 0 {
            offset += written as usize;
        } else if written < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        } else {
            break;
        }
    }
    unsafe {
        libc::fcntl(fd, libc::F_SETFL, flags);
    }
}

pub fn discard_pending_output() {
    // A terminal that never consumes queued output can keep macOS exit pending.
    unsafe {
        libc::tcflush(libc::STDOUT_FILENO, libc::TCOFLUSH);
    }
}

fn emergency_restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = terminal::disable_raw_mode();
    best_effort(libc::STDOUT_FILENO, RESET);
}
