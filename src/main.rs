#[cfg(not(target_os = "macos"))]
compile_error!("tpane currently supports macOS only");

mod app;
mod bus;
mod engine;
mod host;
mod host_input;
mod input;
mod output;
mod pane;
mod process_watch;
mod pty;
mod render;
mod runtime;
mod selection;
mod tab_keys;
mod transport;
mod tree;

use std::{
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use crossterm::{event::Event, terminal};
use futures_util::StreamExt;
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
};

use crate::{
    app::App,
    bus::{Control, PtyEvent},
    host::Host,
    host_input::HostInput,
    output::HostOutput,
};

fn main() -> ExitCode {
    let runtime = runtime::new();
    let result = runtime.block_on(run());
    trace("run returned");
    let code = match result {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            host::best_effort(
                libc::STDERR_FILENO,
                format!("tpane: {error:#}\n").as_bytes(),
            );
            ExitCode::FAILURE
        }
    };
    trace("before runtime drop");
    drop(runtime);
    trace("after runtime drop");
    code
}

async fn run() -> Result<u8> {
    let mut args = std::env::args_os().skip(1);
    let mut shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
    while let Some(arg) = args.next() {
        if arg == "--help" || arg == "-h" {
            println!(
                "tpane — a terminal multiplexer powered by Ghostty\n\nUsage: tpane [--shell /path/to/shell]\n\n[+] adds a tab, [x] closes its subtree. Option+1..9 (0 = 10) selects tabs; hold Option and type digits to descend into nested tabs."
            );
            return Ok(0);
        } else if arg == "--version" || arg == "-V" {
            println!("tpane {}", env!("CARGO_PKG_VERSION"));
            return Ok(0);
        } else if arg == "--shell" {
            shell = PathBuf::from(args.next().context("--shell requires a path")?);
        } else {
            bail!("unknown argument: {}", arg.to_string_lossy());
        }
    }
    trace("starting");
    let mut term = signal(SignalKind::terminate())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut host = Host::enter()?;
    let (sender, mut data, mut control) = bus::channel();
    let mut output = HostOutput::new(libc::STDOUT_FILENO, sender.clone())?;
    output.send_frame(host::setup()?)?;
    let (cols, rows, cell) = dimensions()?;
    let mut app = match App::new(shell, cols, rows, cell, sender) {
        Ok(app) => app,
        Err(error) => {
            if output
                .finish(host::RESET, Duration::from_secs(1))
                .await
                .is_ok()
            {
                host.finish()?;
            }
            return Err(error);
        }
    };
    let mut input = HostInput::new();
    let result = async {
        let mut output_busy = true;
        let mut processed = 0;
        let mut batch = Vec::with_capacity(64);
        let mut turn = Instant::now();
        loop {
            if let Some(code) = app.exit_code() { return Ok(code); }
            let render = app.needs_render() && !output_busy;
            let timers = !app.timers.is_empty();
            let cleanup = !app.cleanup.is_empty();
            tokio::select! {
                biased;
                _ = term.recv() => { trace("SIGTERM"); return Ok(0); },
                _ = hangup.recv() => return Ok(0),
                _ = interrupt.recv() => return Ok(0),
                _ = quit.recv() => return Ok(0),
                _ = std::future::ready(()), if render => {
                    let mut frame = Vec::new();
                    if app.draw(&mut frame)? {
                        output.send_frame(frame)?;
                        output_busy = true;
                    }
                }
                Some(event) = control.recv() => {
                    if let Control::FrameWritten { result } = event {
                        result.context("write host frame")?;
                        output_busy = false;
                    } else { app.control(event)?; }
                }
                Some(expired) = app.timers.next(), if timers => app.timeout(expired.into_inner())?,
                Some(result) = app.cleanup.join_next(), if cleanup => { result.context("pane cleanup task")?; }
                event = next_activity(&mut input.events, &mut data, &mut batch) => match event? {
                    Activity::Input(Event::Resize(_, _)) => {
                        let (cols, rows, cell) = dimensions()?;
                        app.resize(cols, rows, cell)?;
                    }
                    Activity::Input(event) => app.event(event)?,
                    Activity::PtyBatch => {
                        processed += batch.len();
                        for event in batch.drain(..) { app.data(event)?; }
                    }
                }
            }
            processed += 1;
            if processed >= 64 || turn.elapsed() >= Duration::from_millis(2) {
                tokio::task::yield_now().await;
                processed = 0;
                turn = Instant::now();
            }
        }
    }.await;
    trace("dropping input");
    input.shutdown().await;
    drop(data);
    trace("shutting down panes");
    app.shutdown().await;
    trace("finishing output");
    let restored = output.finish(host::RESET, Duration::from_secs(1)).await;
    trace("output finished");
    if restored.is_err() {
        host::discard_pending_output();
        host.finish()?;
        return Ok(1);
    }
    host.finish()?;
    result
}

enum Activity {
    Input(Event),
    PtyBatch,
}

async fn next_activity(
    input: &mut mpsc::Receiver<std::io::Result<Event>>,
    data: &mut mpsc::Receiver<PtyEvent>,
    batch: &mut Vec<PtyEvent>,
) -> Result<Activity> {
    tokio::select! {
        event = input.recv() => Ok(Activity::Input(event.context("terminal input stream ended")??)),
        count = data.recv_many(batch, 64) => {
            anyhow::ensure!(count != 0, "PTY event bus closed");
            Ok(Activity::PtyBatch)
        },
    }
}

fn trace(message: &str) {
    if let Some(path) = std::env::var_os("TPANE_TRACE_FILE") {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{message}");
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
