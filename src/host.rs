use std::{
    io::{self, IsTerminal, Write},
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

pub struct Host;

impl Host {
    pub fn enter() -> Result<Self> {
        ensure!(
            io::stdin().is_terminal() && io::stdout().is_terminal(),
            "tpane requires an interactive terminal on stdin and stdout"
        );
        terminal::enable_raw_mode()?;
        ACTIVE.store(true, Ordering::SeqCst);
        let guard = Self;
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            previous(info);
        }));
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            EnableFocusChange,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
            )
        )?;
        io::stdout().write_all(b"\x1b[?7l\x1b[2J\x1b[H")?;
        io::stdout().flush()?;
        Ok(guard)
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        restore();
    }
}

fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let _ = io::stdout().write_all(b"\x1b[?2026l\x1b[<u\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1015l\x1b[?1004l\x1b[?2004l\x1b[0m\x1b[0 q\x1b[?7h\x1b[?25h\x1b[?1049l");
    let _ = io::stdout().flush();
    let _ = terminal::disable_raw_mode();
}
