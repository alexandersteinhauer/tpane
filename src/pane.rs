use std::{
    io::{self, Write},
    path::Path,
    sync::mpsc::{Receiver, TryRecvError},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
use libghostty_vt::{screen::Screen, terminal::ScrollViewport};

use crate::{
    engine::Engine,
    input::{self, Input},
    io_worker::{self, Writer},
    pty::Session,
    render::Renderer,
    selection::{self, SelectionState},
};

pub struct Pane {
    selection: SelectionState,
    renderer: Renderer,
    input: Input,
    pub engine: Engine,
    output: Receiver<io::Result<Vec<u8>>>,
    writer: Writer,
    pub session: Session,
    exited: Option<(u8, Instant)>,
    eof: Option<Instant>,
    geometry: (u16, u16, u16, u16),
}

pub struct Progress {
    pub consumed: bool,
    pub exit: Option<u8>,
}

impl Pane {
    pub fn new(shell: &Path, cwd: &Path, cols: u16, rows: u16, cell: (u16, u16)) -> Result<Self> {
        let mut engine = Engine::new(cols, rows)?;
        engine.resize(cols, rows, cell.0, cell.1)?;
        let renderer = Renderer::new()?;
        let mut input = Input::new()?;
        input.set_cell_size(cell.0, cell.1);
        let mut session = Session::spawn_in(shell, rows, cols, cwd)?;
        let output = io_worker::read_output(session.reader()?)?;
        let writer = Writer::new(session.take_writer()?)?;
        Ok(Self {
            selection: SelectionState::default(),
            renderer,
            input,
            engine,
            output,
            writer,
            session,
            exited: None,
            eof: None,
            geometry: (cols, rows, cell.0, cell.1),
        })
    }

    pub fn poll(&mut self, read_output: bool) -> Result<Progress> {
        if self.exited.is_none()
            && let Some(status) = self.session.try_wait()?
        {
            self.exited = Some((status.exit_code().min(255) as u8, Instant::now()));
        }
        let mut consumed = false;
        let mut drained = false;
        if read_output {
            match self.output.try_recv() {
                Ok(Ok(bytes)) => {
                    self.engine.feed(&bytes)?;
                    consumed = true;
                }
                Ok(Err(error)) => return Err(error).context("read terminal output"),
                Err(TryRecvError::Empty) => drained = true,
                Err(TryRecvError::Disconnected) => {
                    self.eof.get_or_insert_with(Instant::now);
                    drained = true;
                }
            }
        }
        let replies = self.engine.take_replies();
        if self.exited.is_none() && self.eof.is_none() {
            self.writer.flush()?;
            self.writer.write(&replies)?;
        }
        let exit = if let Some((code, at)) = self.exited {
            (self.eof.is_some() || (drained && at.elapsed() > Duration::from_millis(30)))
                .then_some(code)
        } else {
            self.eof
                .filter(|at| at.elapsed() > Duration::from_millis(100))
                .map(|_| 0)
        };
        Ok(Progress { consumed, exit })
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell: (u16, u16)) -> Result<()> {
        let geometry = (cols, rows, cell.0, cell.1);
        if self.geometry != geometry {
            self.selection.clear(&self.engine.terminal)?;
            self.session.resize(rows, cols)?;
            self.engine.resize(cols, rows, cell.0, cell.1)?;
            self.input.set_cell_size(cell.0, cell.1);
            self.geometry = geometry;
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
                        selection::copy(&text)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn into_session(self) -> Session {
        self.session
    }
}
