//! Headless terminals: a login shell in a PTY with a Ghostty emulator, driven
//! through a `Send + Sync` handle and drawn by any number of frame viewers.
//!
//! # Threading model
//!
//! Each [`Terminal`] has one owner thread (`tpane-terminal`) that holds every
//! `!Send` Ghostty object for it: emulator, renderer, input encoders and
//! selection. The owner runs `Handle::block_on` on the host's multi-thread
//! Tokio runtime, so the PTY reader/writer and exit-watch tasks run on the
//! runtime's workers while the owner parses output or renders. The owner
//! only works when something happens (PTY output, a request, a timer); an
//! idle terminal costs one parked thread.
//!
//! Handles talk to the owner over an unbounded channel: [`Terminal`] (cheap
//! to clone) sends input and requests, each [`Viewer`] pulls frames. Requests
//! are processed in order, so input sent before a text request is reflected
//! in its result once the child has echoed it.
//!
//! # Frames and viewers
//!
//! Frames are rendered lazily, when a viewer asks for one and the screen
//! changed, so output floods coalesce instead of queueing. A viewer's first
//! frame is full; later ones are incremental (see [`crate::frame`]). Frames
//! rendered while a viewer was not asking are queued for it; a viewer that
//! falls more than [`MAX_QUEUED_FRAMES`] behind, or that dropped a pending
//! `next` future, gets a full frame next instead. While the program holds
//! synchronized output (mode 2026), frames wait for it to finish, for at
//! most [`SYNC_HOLD`].
//!
//! # Lifetime
//!
//! The shell's exit is reported as [`TerminalEvent::Exited`] (and by
//! [`Terminal::wait`]); its process tree is cleaned up then, but the screen
//! stays readable and viewable. Dropping the last [`Terminal`] handle, or
//! [`Terminal::shutdown`], ends the owner thread, terminating the shell and
//! its jobs if still running; viewers then get `None`.

use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use tokio::{
    runtime::{Handle, RuntimeFlavor},
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

use crate::{
    bus::{self, Control, PtyEvent},
    event::{InputEvent, Size},
    frame::Frame,
    pane::Pane,
    process_watch::ProcessWatch,
    pty::Command,
    render::FrameMeta,
};

/// Frames queued for a viewer that is not asking before it is reset to a
/// full frame.
pub const MAX_QUEUED_FRAMES: usize = 16;
/// The longest a program's synchronized output holds back frames.
pub const SYNC_HOLD: Duration = Duration::from_millis(500);

const PANE: crate::Id = 1;

pub struct TerminalOptions {
    pub command: Command,
    pub size: Size,
    /// Scrollback limit in bytes; Ghostty prunes whole pages.
    pub scrollback_bytes: usize,
    /// The runtime for PTY I/O tasks; the current one if `None`.
    pub runtime: Option<Handle>,
}

impl TerminalOptions {
    /// A login shell in `cwd`, 4 MiB of scrollback, the current runtime.
    pub fn new(shell: impl Into<PathBuf>, cwd: impl Into<PathBuf>, size: Size) -> Self {
        Self {
            command: Command::login_shell(shell, cwd),
            size,
            scrollback_bytes: 4 * 1024 * 1024,
            runtime: None,
        }
    }
}

/// Things the host should act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalEvent {
    /// The user completed a mouse selection: put the text on the clipboard.
    Copy(String),
    Bell,
    /// A request failed without ending the terminal (e.g. more than 8 MiB of
    /// input pending for a stopped child).
    Error(String),
    /// The shell exited. The screen remains readable.
    Exited(ExitStatus),
}

/// How the shell ended. `code` is `None` when it could not be reaped in time
/// (the PTY closed first); `signal` names a terminating signal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: Option<u32>,
    pub signal: Option<String>,
}

enum Request {
    Input(InputEvent),
    Attach(u64),
    Detach(u64),
    Next {
        viewer: u64,
        reply: oneshot::Sender<Arc<Frame>>,
    },
    Text {
        scrollback: bool,
        reply: oneshot::Sender<Result<String>>,
    },
    Shutdown,
}

struct Inner {
    requests: mpsc::UnboundedSender<Request>,
    pid: u32,
    exit: watch::Receiver<Option<ExitStatus>>,
    done: watch::Receiver<bool>,
    viewers: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Shutdown);
    }
}

/// A handle to a headless terminal. `Send + Sync`; clones share the terminal.
#[derive(Clone)]
pub struct Terminal {
    inner: Arc<Inner>,
}

impl Terminal {
    /// Starts the command in a new PTY on a new owner thread. Needs a
    /// multi-thread Tokio runtime (`options.runtime` or the current one).
    /// Events arrive on the returned receiver; dropping it is fine.
    pub async fn spawn(
        options: TerminalOptions,
    ) -> Result<(Self, mpsc::UnboundedReceiver<TerminalEvent>)> {
        let runtime = match options.runtime.clone() {
            Some(runtime) => runtime,
            None => Handle::try_current().context("Terminal::spawn needs a Tokio runtime")?,
        };
        if runtime.runtime_flavor() == RuntimeFlavor::CurrentThread {
            bail!("Terminal::spawn needs a multi-thread Tokio runtime for PTY I/O");
        }
        let (requests, receiver) = mpsc::unbounded_channel();
        let (events, event_receiver) = mpsc::unbounded_channel();
        let (exit_sender, exit) = watch::channel(None);
        let (done_sender, done) = watch::channel(false);
        let (ready, started) = oneshot::channel();
        std::thread::Builder::new()
            .name("tpane-terminal".into())
            .spawn(move || {
                let thread_runtime = runtime.clone();
                thread_runtime.block_on(async move {
                    let owner = match Owner::start(options, events, exit_sender) {
                        Ok(owner) => owner,
                        Err(error) => {
                            let _ = ready.send(Err(error));
                            return;
                        }
                    };
                    let _ = ready.send(Ok(owner.pid()));
                    owner.run(receiver).await;
                });
                let _ = done_sender.send(true);
            })
            .context("starting the terminal owner thread")?;
        let pid = started
            .await
            .map_err(|_| anyhow!("terminal owner thread ended during startup"))??;
        Ok((
            Self {
                inner: Arc::new(Inner {
                    requests,
                    pid,
                    exit,
                    done,
                    viewers: AtomicU64::new(0),
                }),
            },
            event_receiver,
        ))
    }

    /// The shell's process ID.
    pub fn pid(&self) -> u32 {
        self.inner.pid
    }

    /// Queues an input event. Fails only once the terminal is shut down;
    /// after the shell exited, input is ignored.
    pub fn input(&self, event: InputEvent) -> Result<()> {
        self.request(Request::Input(event))
    }

    pub fn resize(&self, size: Size) -> Result<()> {
        self.input(InputEvent::Resize(size))
    }

    /// A new viewer; its first frame is full.
    pub fn attach(&self) -> Result<Viewer> {
        let id = self.inner.viewers.fetch_add(1, Ordering::Relaxed);
        self.request(Request::Attach(id))?;
        Ok(Viewer {
            id,
            requests: self.inner.requests.clone(),
        })
    }

    /// The screen (the active area, wherever the viewport is scrolled) as
    /// plain text, by Ghostty's formatter.
    pub async fn screen_text(&self) -> Result<String> {
        self.text(false).await
    }

    /// Scrollback and screen as plain text, soft-wrapped lines joined.
    pub async fn scrollback_text(&self) -> Result<String> {
        self.text(true).await
    }

    async fn text(&self, scrollback: bool) -> Result<String> {
        let (reply, result) = oneshot::channel();
        self.request(Request::Text { scrollback, reply })?;
        result.await.map_err(|_| anyhow!("terminal is shut down"))?
    }

    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.inner.exit.borrow().clone()
    }

    /// Waits for the shell to exit. Fails if the terminal is shut down
    /// first.
    pub async fn wait(&self) -> Result<ExitStatus> {
        let mut exit = self.inner.exit.clone();
        let status = exit
            .wait_for(Option::is_some)
            .await
            .map_err(|_| anyhow!("terminal shut down before the shell exited"))?;
        Ok(status.clone().expect("waited for an exit status"))
    }

    /// Terminates the shell and its jobs if still running and ends the
    /// owner thread, waiting for both.
    pub async fn shutdown(&self) {
        let _ = self.inner.requests.send(Request::Shutdown);
        let mut done = self.inner.done.clone();
        let _ = done.wait_for(|done| *done).await;
    }

    fn request(&self, request: Request) -> Result<()> {
        self.inner
            .requests
            .send(request)
            .map_err(|_| anyhow!("terminal is shut down"))
    }
}

/// Pulls frames for one viewer. Dropping it detaches.
pub struct Viewer {
    id: u64,
    requests: mpsc::UnboundedSender<Request>,
}

impl Viewer {
    /// The next frame: full first, then incremental, waiting until something
    /// changed. `None` once the terminal is shut down. Cancel-safe: dropping
    /// the future makes the following frame a full one.
    pub async fn next(&mut self) -> Option<Arc<Frame>> {
        let (reply, frame) = oneshot::channel();
        self.requests
            .send(Request::Next {
                viewer: self.id,
                reply,
            })
            .ok()?;
        frame.await.ok()
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        let _ = self.requests.send(Request::Detach(self.id));
    }
}

#[derive(Default)]
struct Slot {
    queue: VecDeque<Arc<Frame>>,
    /// The viewer needs a full frame next.
    behind: bool,
    waiting: Option<oneshot::Sender<Arc<Frame>>>,
}

struct Owner {
    pane: Pane,
    watch: ProcessWatch,
    data: mpsc::Receiver<PtyEvent>,
    control: mpsc::UnboundedReceiver<Control>,
    events: mpsc::UnboundedSender<TerminalEvent>,
    exit: watch::Sender<Option<ExitStatus>>,
    exited: bool,
    viewers: BTreeMap<u64, Slot>,
    seq: u64,
    meta: Option<FrameMeta>,
    /// Something may have changed since the last frame.
    changed: bool,
    /// When the program began holding synchronized output.
    sync_since: Option<Instant>,
}

impl Owner {
    fn start(
        options: TerminalOptions,
        events: mpsc::UnboundedSender<TerminalEvent>,
        exit: watch::Sender<Option<ExitStatus>>,
    ) -> Result<Self> {
        let (bus, data, control) = bus::channel();
        let mut watch = ProcessWatch::new(bus.clone())?;
        let size = options.size;
        let mut pane = Pane::new(
            &options.command,
            PANE,
            (
                size.cols.max(1),
                size.rows.max(1),
                (size.cell_width, size.cell_height),
            ),
            bus,
            &mut watch,
        )?;
        pane.engine_mut()
            .terminal
            .set_scrollback_max_bytes(Some(options.scrollback_bytes))?;
        Ok(Self {
            pane,
            watch,
            data,
            control,
            events,
            exit,
            exited: false,
            viewers: BTreeMap::new(),
            seq: 0,
            meta: None,
            changed: true,
            sync_since: None,
        })
    }

    fn pid(&self) -> u32 {
        self.pane.session.pid() as u32
    }

    async fn run(mut self, mut requests: mpsc::UnboundedReceiver<Request>) {
        let mut batch = Vec::with_capacity(64);
        let mut data_open = true;
        let result: Result<()> = async {
            loop {
                let wanted = self.changed && self.viewers.values().any(|slot| slot.waiting.is_some());
                let sync_until = self.sync_since.map(|since| since + SYNC_HOLD);
                let render = wanted && sync_until.is_none_or(|until| Instant::now() >= until);
                let deadline = (!self.exited)
                    .then(|| self.pane.deadline())
                    .flatten()
                    .map(Instant::from_std);
                tokio::select! {
                    biased;
                    request = requests.recv() => match request {
                        None | Some(Request::Shutdown) => return Ok(()),
                        Some(request) => self.request(request)?,
                    },
                    Some(event) = self.control.recv() => self.pane_control(event)?,
                    count = self.data.recv_many(&mut batch, 64), if data_open => {
                        if count == 0 {
                            data_open = false;
                        }
                        for event in batch.drain(..) {
                            self.data(event)?;
                        }
                    }
                    _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                        self.timeout()?;
                    }
                    _ = tokio::time::sleep_until(sync_until.unwrap_or_else(Instant::now)), if wanted && !render => {}
                    _ = std::future::ready(()), if render => self.broadcast()?,
                }
            }
        }
        .await;
        if let Err(error) = result {
            let _ = self.events.send(TerminalEvent::Error(format!("{error:#}")));
        }
        let _ = self.watch.unwatch(self.pane.session.pid());
        self.pane.session.terminate();
    }

    fn request(&mut self, request: Request) -> Result<()> {
        match request {
            Request::Input(event) => {
                if self.exited {
                    return Ok(());
                }
                self.changed = true;
                match self.pane.input(&event) {
                    Ok(Some(text)) => {
                        let _ = self.events.send(TerminalEvent::Copy(text));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = self.events.send(TerminalEvent::Error(format!("{error:#}")));
                    }
                }
            }
            Request::Attach(id) => {
                self.viewers.insert(
                    id,
                    Slot {
                        behind: true,
                        ..Slot::default()
                    },
                );
            }
            Request::Detach(id) => {
                self.viewers.remove(&id);
            }
            Request::Next { viewer, reply } => self.next(viewer, reply)?,
            Request::Text { scrollback, reply } => {
                let engine = self.pane.engine();
                let _ = reply.send(if scrollback {
                    engine.all_text(true)
                } else {
                    engine.screen_text()
                });
            }
            Request::Shutdown => unreachable!("handled by the loop"),
        }
        Ok(())
    }

    fn next(&mut self, viewer: u64, reply: oneshot::Sender<Arc<Frame>>) -> Result<()> {
        let Some(slot) = self.viewers.get_mut(&viewer) else {
            return Ok(());
        };
        if !slot.behind {
            if let Some(frame) = slot.queue.pop_front() {
                if reply.send(frame).is_err() {
                    slot.behind = true;
                    slot.queue.clear();
                }
            } else {
                slot.waiting = Some(reply);
            }
            return Ok(());
        }
        // Bring every other viewer up to date first, so the full frame is
        // the state at `seq` and incremental frames continue from it.
        if self.changed {
            self.broadcast()?;
        }
        let mut frame = self
            .pane
            .frame(true, true)?
            .expect("a forced frame always renders");
        frame.seq = self.seq;
        let slot = self.viewers.get_mut(&viewer).expect("checked above");
        slot.queue.clear();
        slot.behind = reply.send(Arc::new(frame)).is_err();
        Ok(())
    }

    /// Renders pending changes once and hands the frame to every viewer.
    fn broadcast(&mut self) -> Result<()> {
        self.changed = false;
        let meta = FrameMeta::read(&self.pane.engine().terminal)?;
        let resized = self
            .meta
            .as_ref()
            .is_none_or(|last| (last.cols, last.rows) != (meta.cols, meta.rows));
        let always = self.meta.as_ref() != Some(&meta);
        let Some(mut frame) = self.pane.frame_with(meta.clone(), resized, always)? else {
            return Ok(());
        };
        self.seq += 1;
        frame.seq = self.seq;
        self.meta = Some(meta);
        let frame = Arc::new(frame);
        for slot in self.viewers.values_mut() {
            if slot.behind {
                continue;
            }
            if let Some(reply) = slot.waiting.take() {
                if reply.send(Arc::clone(&frame)).is_err() {
                    slot.behind = true;
                }
            } else if slot.queue.len() >= MAX_QUEUED_FRAMES {
                slot.queue.clear();
                slot.behind = true;
            } else {
                slot.queue.push_back(Arc::clone(&frame));
            }
        }
        Ok(())
    }

    fn data(&mut self, event: PtyEvent) -> Result<()> {
        match event {
            PtyEvent::Output { bytes, .. } => {
                self.changed = true;
                self.pane.output(&bytes)?;
                if self.pane.engine_mut().take_bell() {
                    let _ = self.events.send(TerminalEvent::Bell);
                }
                let sync = self
                    .pane
                    .engine()
                    .terminal
                    .mode(crate::vt::terminal::Mode::SYNC_OUTPUT)?;
                match (sync, self.sync_since) {
                    (true, None) => self.sync_since = Some(Instant::now()),
                    (false, Some(_)) => self.sync_since = None,
                    _ => {}
                }
            }
            PtyEvent::Eof(_) | PtyEvent::ReadError { .. } => {
                self.pane.eof();
                self.settle();
            }
        }
        Ok(())
    }

    fn pane_control(&mut self, event: Control) -> Result<()> {
        match event {
            Control::ChildExited(_) => self.reap(),
            Control::WriteError { error, .. } => {
                if !self.exited && self.pane.session.try_wait()?.is_none() {
                    let _ = self
                        .events
                        .send(TerminalEvent::Error(format!("writing to the PTY: {error}")));
                }
                self.reap();
            }
        }
        Ok(())
    }

    fn timeout(&mut self) -> Result<()> {
        if self.pane.retry_due() {
            self.reap();
        } else {
            self.settle();
        }
        Ok(())
    }

    fn reap(&mut self) {
        if self.exited {
            return;
        }
        if let Err(error) = self.pane.reap(&mut self.watch) {
            let _ = self.events.send(TerminalEvent::Error(format!("{error:#}")));
            self.finish();
            return;
        }
        self.settle();
    }

    fn settle(&mut self) {
        if !self.exited && self.pane.closed().is_some() {
            self.finish();
        }
    }

    fn finish(&mut self) {
        self.exited = true;
        self.changed = true;
        let status = self.pane.session.try_wait().ok().flatten();
        let status = ExitStatus {
            code: status.as_ref().map(|status| status.exit_code()),
            signal: status
                .as_ref()
                .and_then(|status| status.signal().map(str::to_owned)),
        };
        let _ = self.watch.unwatch(self.pane.session.pid());
        // Orphaned jobs end with the shell, as when tpane closes a tab.
        self.pane.session.terminate();
        let _ = self.exit.send(Some(status.clone()));
        let _ = self.events.send(TerminalEvent::Exited(status));
    }
}
