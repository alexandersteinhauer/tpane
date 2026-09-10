#[cfg(not(target_os = "macos"))]
compile_error!("tpane currently supports macOS only");

mod app;
mod engine;
mod host;
mod input;
mod io_worker;
mod pane;
mod pty;
mod render;
mod selection;
mod tree;

use std::{
    io,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event},
    terminal,
};

use crate::{app::App, host::Host};

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("tpane: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<u8> {
    let mut args = std::env::args_os().skip(1);
    let mut shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!(
                "tpane — nested terminal tabs\n\nUsage: tpane [--shell /path/to/shell]\n\n[+]: add a tab; every terminal has its own bar for nesting\nClick a tab to select it; [x] closes its subtree\nMouse wheel: scroll history\nDrag left mouse: select and copy on release\nAlt + mouse: local selection/scrolling in mouse-capturing applications\nExiting a shell closes its tab. No multiplexer keyboard shortcuts."
            );
            return Ok(0);
        } else if arg == "--version" || arg == "-V" {
            println!("tpane {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        } else if arg == "--shell" {
            shell = PathBuf::from(args.next().context("--shell requires a path")?);
        } else {
            bail!("unknown argument: {} (see --help)", arg.to_string_lossy());
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGHUP,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGQUIT,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    let _host = Host::enter()?;
    let (cols, rows, cell) = dimensions()?;
    let mut app = App::new(shell, cols, rows, cell)?;
    let mut last_frame = Instant::now() - Duration::from_millis(17);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(0);
        }
        if let Some(code) = app.tick()? {
            return Ok(code);
        }
        if last_frame.elapsed() >= Duration::from_millis(16) {
            app.draw(&mut io::stdout())?;
            last_frame = Instant::now();
        }
        if event::poll(Duration::from_millis(4))? {
            for _ in 0..32 {
                match event::read()? {
                    Event::Resize(_, _) => {
                        let (cols, rows, cell) = dimensions()?;
                        app.resize(cols, rows, cell)?;
                    }
                    event => app.event(event)?,
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

fn dimensions() -> Result<(u16, u16, (u16, u16))> {
    let (cols, rows) = terminal::size()?;
    let (cols, rows) = (cols.max(1), rows.max(1));
    let cell = terminal::window_size()
        .map(|size| (size.width / cols, size.height / rows))
        .unwrap_or((0, 0));
    Ok((cols, rows, cell))
}
