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
mod wake;

use std::{
    io,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::TryRecvError,
    },
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossterm::{event::Event, terminal};

use crate::{app::App, host::Host, wake::Wake};

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
    let wake = Wake::new();
    let input = io_worker::read_input(wake.clone())?;
    let mut app = App::new(shell, cols, rows, cell, wake.clone())?;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(0);
        }
        let mut input_budget_exhausted = true;
        for _ in 0..128 {
            match input.try_recv() {
                Ok(Ok(Event::Resize(_, _))) => {
                    let (cols, rows, cell) = dimensions()?;
                    app.resize(cols, rows, cell)?;
                }
                Ok(Ok(event)) => app.event(event)?,
                Ok(Err(error)) => return Err(error).context("read terminal input"),
                Err(TryRecvError::Empty) => {
                    input_budget_exhausted = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => bail!("terminal input worker stopped"),
            }
        }
        let (exit, more_output) = app.tick()?;
        if let Some(code) = exit {
            return Ok(code);
        }
        if app.needs_render() {
            app.draw(&mut io::stdout())?;
        }
        if more_output || input_budget_exhausted {
            continue;
        }
        let timeout = Duration::from_millis(100);
        wake.wait(timeout);
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
