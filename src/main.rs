#[cfg(not(target_os = "macos"))]
compile_error!("tpane currently supports macOS only");

mod engine;
mod host;
mod input;
mod io_worker;
mod pty;
mod render;
mod selection;

use std::{
    io,
    path::PathBuf,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::TryRecvError,
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{
    event::{self, Event, KeyModifiers, MouseEventKind},
    terminal,
};
use libghostty_vt::{screen::Screen, terminal::ScrollViewport};

use crate::{
    engine::Engine, host::Host, input::Input, pty::Session, render::Renderer,
    selection::SelectionState,
};

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
                "tpane — single-terminal MVP\n\nUsage: tpane [--shell /path/to/shell]\n\nMouse wheel: scroll history\nDrag left mouse: select and copy on release\nAlt + mouse: use local selection/scrolling when an application captures the mouse\nExit the shell to close tpane. No multiplexer keyboard shortcuts."
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
    let (cols, rows) = terminal::size()?;
    let (cols, rows) = (cols.max(1), rows.max(1));
    let mut engine = Engine::new(cols, rows)?;
    let mut renderer = Renderer::new()?;
    let mut input = Input::new()?;
    resize(&mut engine, &mut input, cols, rows)?;
    let mut selection = SelectionState::default();
    let mut session = Session::spawn(&shell, rows, cols)?;
    let output = io_worker::read_output(session.reader()?)?;
    let mut writer = io_worker::Writer::new(session.take_writer()?)?;
    let mut last_frame = Instant::now() - Duration::from_millis(17);
    let mut exited = None;
    let mut eof = None;

    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(0);
        }
        writer.flush()?;
        let mut drained = false;
        for _ in 0..4 {
            match output.try_recv() {
                Ok(Ok(bytes)) => {
                    engine.feed(&bytes)?;
                }
                Ok(Err(error)) => return Err(error).context("read terminal output"),
                Err(TryRecvError::Empty) => {
                    drained = true;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    eof.get_or_insert_with(Instant::now);
                    drained = true;
                    break;
                }
            }
        }
        writer.write(&engine.take_replies())?;
        if last_frame.elapsed() >= Duration::from_millis(16) {
            renderer.draw(&engine.terminal, &mut io::stdout())?;
            last_frame = Instant::now();
        }
        if exited.is_none()
            && let Some(status) = session.try_wait()?
        {
            exited = Some((status.exit_code().min(255) as u8, Instant::now()));
        }
        if let Some((code, at)) = exited
            && (eof.is_some() || (drained && at.elapsed() > Duration::from_millis(30)))
        {
            renderer.draw(&engine.terminal, &mut io::stdout())?;
            return Ok(code);
        }
        if eof.is_some_and(|at| at.elapsed() > Duration::from_millis(100)) && exited.is_none() {
            return Ok(0);
        }
        if event::poll(Duration::from_millis(4))? {
            for _ in 0..32 {
                match event::read()? {
                    Event::Key(key) => {
                        let bytes = input.key(&engine.terminal, key)?;
                        if !bytes.is_empty() {
                            selection.clear(&engine.terminal)?;
                            engine.terminal.scroll_viewport(ScrollViewport::Bottom);
                            writer.write(&bytes)?;
                        }
                    }
                    Event::Paste(text) => {
                        selection.clear(&engine.terminal)?;
                        engine.terminal.scroll_viewport(ScrollViewport::Bottom);
                        writer.write(&input::paste(&engine.terminal, &text)?)?;
                    }
                    Event::Mouse(mouse) => {
                        if engine.terminal.is_mouse_tracking()?
                            && !mouse.modifiers.contains(KeyModifiers::ALT)
                        {
                            writer.write(&input.mouse(&engine.terminal, mouse)?)?;
                        } else {
                            match mouse.kind {
                                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                                    if engine.terminal.active_screen()? == Screen::Primary =>
                                {
                                    let delta = if mouse.kind == MouseEventKind::ScrollUp {
                                        -3
                                    } else {
                                        3
                                    };
                                    engine
                                        .terminal
                                        .scroll_viewport(ScrollViewport::Delta(delta));
                                }
                                _ => {
                                    if let Some(text) =
                                        selection.mouse(&mut engine.terminal, mouse)?
                                    {
                                        selection::copy(&text)?;
                                    }
                                }
                            }
                        }
                    }
                    Event::Resize(cols, rows) => {
                        let (cols, rows) = (cols.max(1), rows.max(1));
                        selection.clear(&engine.terminal)?;
                        resize(&mut engine, &mut input, cols, rows)?;
                        session.resize(rows, cols)?;
                    }
                    Event::FocusGained => writer.write(&input::focus(&engine.terminal, true)?)?,
                    Event::FocusLost => writer.write(&input::focus(&engine.terminal, false)?)?,
                }
                if !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
    }
}

fn resize(engine: &mut Engine, input: &mut Input, cols: u16, rows: u16) -> Result<()> {
    let (width, height) = terminal::window_size()
        .map(|size| (size.width / cols, size.height / rows))
        .unwrap_or((0, 0));
    engine.resize(cols, rows, width, height)?;
    input.set_cell_size(width, height);
    Ok(())
}
