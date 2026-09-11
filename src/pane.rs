use std::{
    io::Write,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
use libghostty_vt::{screen::Screen, terminal::ScrollViewport};

use crate::{
    bus::{Bus, Control},
    engine::Engine,
    input::{self, Input},
    process_watch::ProcessWatch,
    pty::Session,
    render::Renderer,
    selection::SelectionState,
    transport::{self, Writer},
    tree::Id,
};

pub struct Pane {
    selection: SelectionState,
    renderer: Renderer,
    input: Input,
    engine: Engine,
    writer: Writer,
    _tasks: transport::Tasks,
    pub session: Session,
    exit_code: Option<u8>,
    eof: bool,
    close_at: Option<Instant>,
    retry_at: Option<Instant>,
    exit_reported: Option<Instant>,
    geometry: (u16, u16, u16, u16),
    bus: Bus,
}

impl Pane {
    pub fn new(
        shell: &Path,
        cwd: &Path,
        id: Id,
        geometry: (u16, u16, (u16, u16)),
        bus: Bus,
        watch: &mut ProcessWatch,
    ) -> Result<Self> {
        let (cols, rows, cell) = geometry;
        let mut engine = Engine::new(cols, rows)?;
        engine.resize(cols, rows, cell.0, cell.1)?;
        let renderer = Renderer::new()?;
        let mut input = Input::new()?;
        input.set_cell_size(cell.0, cell.1);
        let session = Session::spawn_in(shell, rows, cols, cwd)?;
        let connection = transport::connect(session.raw_fd()?, id, bus.clone())?;
        watch.watch(session.pid(), id)?;
        Ok(Self {
            selection: SelectionState::default(),
            renderer,
            input,
            engine,
            writer: connection.writer,
            _tasks: connection.tasks,
            session,
            exit_code: None,
            eof: false,
            close_at: None,
            retry_at: None,
            exit_reported: None,
            geometry: (cols, rows, cell.0, cell.1),
            bus,
        })
    }

    pub fn output(&mut self, bytes: &[u8]) -> Result<()> {
        self.engine.feed(bytes)?;
        self.replies()
    }

    fn replies(&mut self) -> Result<()> {
        let replies = self.engine.take_replies();
        if !self.eof && self.exit_code.is_none() {
            self.writer.write(&replies)?;
        }
        Ok(())
    }

    pub fn eof(&mut self) {
        self.eof = true;
        self.close_at
            .get_or_insert_with(|| Instant::now() + Duration::from_millis(100));
    }

    pub fn reap(&mut self, watch: &mut ProcessWatch) -> Result<()> {
        let now = Instant::now();
        if self.exit_code.is_some() {
            return Ok(());
        }
        if let Some(status) = self.session.try_wait()? {
            watch.unwatch(self.session.pid())?;
            self.exit_code = Some(status.exit_code().min(255) as u8);
            self.retry_at = None;
            self.close_at = Some(now + Duration::from_millis(30));
        } else {
            let reported = self.exit_reported.get_or_insert(now);
            if now.duration_since(*reported) >= Duration::from_millis(100) {
                bail!("exit notification did not become reapable");
            }
            self.retry_at = Some(now + Duration::from_millis(1));
        }
        Ok(())
    }

    pub fn retry_due(&self) -> bool {
        self.retry_at.is_some_and(|at| Instant::now() >= at)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.close_at.into_iter().chain(self.retry_at).min()
    }

    pub fn closed(&self) -> Option<u8> {
        if self.eof && self.exit_code.is_some()
            || self.close_at.is_some_and(|at| Instant::now() >= at)
        {
            Some(self.exit_code.unwrap_or(0))
        } else {
            None
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell: (u16, u16)) -> Result<()> {
        let geometry = (cols, rows, cell.0, cell.1);
        if self.geometry != geometry {
            self.selection.clear(&self.engine.terminal)?;
            self.session.resize(rows, cols)?;
            self.engine.resize(cols, rows, cell.0, cell.1)?;
            self.input.set_cell_size(cell.0, cell.1);
            self.geometry = geometry;
            self.replies()?;
        }
        Ok(())
    }

    pub fn draw(&mut self, output: &mut impl Write, top: u16, force: bool) -> Result<()> {
        self.renderer
            .draw_at(&self.engine.terminal, output, top, force)
    }

    pub fn focus(&mut self, focused: bool) -> Result<()> {
        if !focused {
            self.selection.cancel_drag();
        }
        self.writer
            .write(&input::focus(&self.engine.terminal, focused)?)
    }

    pub fn event(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Key(key) => {
                let bytes = self.input.key(&self.engine.terminal, key)?;
                if !bytes.is_empty() {
                    self.selection.clear(&self.engine.terminal)?;
                    self.engine.terminal.scroll_viewport(ScrollViewport::Bottom);
                    self.writer.write(&bytes)?;
                }
            }
            Event::Paste(text) => {
                self.selection.clear(&self.engine.terminal)?;
                self.engine.terminal.scroll_viewport(ScrollViewport::Bottom);
                self.writer
                    .write(&input::paste(&self.engine.terminal, &text)?)?;
            }
            Event::Mouse(mouse) => self.mouse(mouse)?,
            _ => {}
        }
        Ok(())
    }

    fn mouse(&mut self, mouse: MouseEvent) -> Result<()> {
        if self.engine.terminal.is_mouse_tracking()? && !mouse.modifiers.contains(KeyModifiers::ALT)
        {
            self.writer
                .write(&self.input.mouse(&self.engine.terminal, mouse)?)?;
        } else {
            match mouse.kind {
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    if self.engine.terminal.active_screen()? == Screen::Primary =>
                {
                    let delta = if mouse.kind == MouseEventKind::ScrollUp {
                        -3
                    } else {
                        3
                    };
                    self.engine
                        .terminal
                        .scroll_viewport(ScrollViewport::Delta(delta));
                }
                _ => {
                    if let Some(text) = self.selection.mouse(&mut self.engine.terminal, mouse)? {
                        self.copy(text)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn copy(&self, text: Vec<u8>) -> Result<()> {
        let permit = self
            .bus
            .clipboard
            .clone()
            .try_acquire_owned()
            .map_err(|_| anyhow::anyhow!("clipboard is busy"))?;
        let control = self.bus.control.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let result: Result<()> = async {
                let mut child = tokio::process::Command::new("/usr/bin/pbcopy")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn()?;
                let operation = async {
                    let mut stdin = child
                        .stdin
                        .take()
                        .ok_or_else(|| anyhow::anyhow!("clipboard input unavailable"))?;
                    stdin.write_all(&text).await?;
                    drop(stdin);
                    anyhow::ensure!(child.wait().await?.success(), "clipboard command failed");
                    Ok::<_, anyhow::Error>(())
                };
                tokio::time::timeout(Duration::from_secs(1), operation).await??;
                Ok(())
            }
            .await;
            drop(permit);
            let _ = control.send(Control::ClipboardFinished(
                result.map_err(|error| error.to_string()),
            ));
        });
        Ok(())
    }

    pub fn into_session(self) -> Session {
        self.session
    }
}
