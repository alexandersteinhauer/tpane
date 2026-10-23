use std::{
    collections::BTreeMap,
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use crossterm::event::{Event, KeyCode, KeyEventKind, MouseEvent, MouseEventKind};
use tokio::task::JoinSet;
use tokio_util::time::{DelayQueue, delay_queue::Key};

use crate::{
    bus::{Bus, Control, PtyEvent},
    command_keys::{CommandKeys, Route as CommandRoute},
    explorer::{Command, Explorer},
    pane::Pane,
    process_watch::ProcessWatch,
    tab_keys::{Route, TabKeys},
    tree::{Action, Id, Tree},
};

pub struct App {
    tree: Tree,
    tab_keys: TabKeys,
    command_keys: CommandKeys,
    explorer: Option<Explorer>,
    names: BTreeMap<Id, String>,
    // Modal action releases must not reach the newly focused terminal.
    swallowed: Vec<KeyCode>,
    panes: BTreeMap<Id, Pane>,
    shell: PathBuf,
    launch_directory: PathBuf,
    next_id: Id,
    cols: u16,
    rows: u16,
    cell: (u16, u16),
    dirty: bool,
    content_dirty: bool,
    // Frame staging buffer, reused across draws: frames recur hundreds of
    // times per second under output flood, where per-frame allocation shows
    // up in profiles.
    frame: Vec<u8>,
    bus: Bus,
    watch: ProcessWatch,
    focused: bool,
    capture: Option<Capture>,
    confirm: Option<Id>,
    notice: Option<(String, Instant)>,
    pub cleanup: JoinSet<()>,
    pub timers: DelayQueue<Timeout>,
    deadlines: BTreeMap<Id, Key>,
    notice_key: Option<Key>,
    exit_code: u8,
}

#[derive(Clone, Copy)]
pub enum Timeout {
    Pane(Id),
    Notice,
}

#[derive(Clone, Copy)]
enum Capture {
    Terminal(Id),
    Chrome,
}

impl App {
    pub fn new(shell: PathBuf, cols: u16, rows: u16, cell: (u16, u16), bus: Bus) -> Result<Self> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let launch_directory = std::env::current_dir()?;
        let tree = Tree::new(1);
        let mut watch = ProcessWatch::new(bus.clone())?;
        let pane = Pane::new(
            &shell,
            &launch_directory,
            1,
            (cols, rows, cell),
            bus.clone(),
            &mut watch,
        )?;
        Ok(Self {
            tree,
            tab_keys: TabKeys::default(),
            command_keys: CommandKeys::default(),
            explorer: None,
            names: BTreeMap::new(),
            swallowed: Vec::new(),
            panes: BTreeMap::from([(1, pane)]),
            shell,
            launch_directory,
            next_id: 2,
            cols,
            rows,
            cell,
            dirty: true,
            content_dirty: true,
            frame: Vec::new(),
            bus,
            watch,
            focused: true,
            capture: None,
            confirm: None,
            notice: None,
            cleanup: JoinSet::new(),
            timers: DelayQueue::new(),
            deadlines: BTreeMap::new(),
            notice_key: None,
            exit_code: 0,
        })
    }

    pub fn data(&mut self, event: PtyEvent) -> Result<()> {
        match event {
            PtyEvent::Output { id, bytes, _credit } => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    let result = pane.output(&bytes);
                    self.content_dirty |= self.tree.active_terminal() == Some(id);
                    if let Err(error) = result {
                        self.failed(id, error.to_string())?;
                    }
                }
            }
            PtyEvent::Eof(id) => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    pane.eof();
                }
                self.settle(id)?;
            }
            PtyEvent::ReadError { id, error } => self.failed(id, error.to_string())?,
        }
        Ok(())
    }

    pub fn control(&mut self, event: Control) -> Result<()> {
        match event {
            Control::ChildExited(id) => {
                if let Some(pane) = self.panes.get_mut(&id)
                    && let Err(error) = pane.reap(&mut self.watch)
                {
                    self.failed(id, error.to_string())?;
                }
                self.settle(id)?;
            }
            Control::WriteError { id, error } => {
                if let Some(pane) = self.panes.get_mut(&id) {
                    if pane.session.try_wait()?.is_some() {
                        pane.reap(&mut self.watch)?;
                        self.settle(id)?;
                    } else {
                        self.failed(id, error.to_string())?;
                    }
                }
            }
            Control::ClipboardFinished(result) => {
                if let Err(error) = result {
                    self.notify(format!("Clipboard: {error}"));
                }
            }
            Control::FrameWritten { .. } => {
                unreachable!("host completion belongs to the coordinator")
            }
        }
        Ok(())
    }

    fn settle(&mut self, id: Id) -> Result<()> {
        let Some(pane) = self.panes.get(&id) else {
            return Ok(());
        };
        if let Some(code) = pane.closed() {
            self.exit_code = code;
            return self.remove(id);
        }
        if let Some(deadline) = pane.deadline() {
            if let Some(key) = self.deadlines.get(&id) {
                self.timers.reset_at(key, deadline.into());
            } else {
                let key = self.timers.insert_at(Timeout::Pane(id), deadline.into());
                self.deadlines.insert(id, key);
            }
        }
        Ok(())
    }

    pub fn timeout(&mut self, event: Timeout) -> Result<()> {
        match event {
            Timeout::Notice => {
                self.notice_key = None;
                self.notice = None;
                self.dirty = true;
            }
            Timeout::Pane(id) => {
                self.deadlines.remove(&id);
                if let Some(pane) = self.panes.get_mut(&id)
                    && pane.retry_due()
                    && let Err(error) = pane.reap(&mut self.watch)
                {
                    self.failed(id, error.to_string())?;
                }
                self.settle(id)?;
            }
        }
        Ok(())
    }

    fn failed(&mut self, id: Id, message: String) -> Result<()> {
        if !self.panes.contains_key(&id) {
            return Ok(());
        }
        self.exit_code = 1;
        self.remove(id)?;
        if self.tree.root.is_none() {
            bail!(message);
        }
        self.notify(format!("Shell {id}: {message}"));
        Ok(())
    }

    pub fn exit_code(&self) -> Option<u8> {
        self.tree.root.is_none().then_some(self.exit_code)
    }

    pub async fn shutdown(&mut self) {
        let mut sessions = Vec::new();
        for (_, pane) in std::mem::take(&mut self.panes) {
            let _ = self.watch.unwatch(pane.session.pid());
            sessions.push(pane.into_session());
        }
        if !sessions.is_empty() {
            self.cleanup.spawn_blocking(move || drop(sessions));
        }
        while self.cleanup.join_next().await.is_some() {}
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell: (u16, u16)) -> Result<()> {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.cell = cell;
        self.tab_keys.reset();
        self.command_keys.reset();
        self.capture = None;
        self.layout_panes()?;
        self.dirty = true;
        Ok(())
    }

    fn layout_panes(&mut self) -> Result<()> {
        for pane in self.panes.values_mut() {
            pane.resize(self.cols, self.rows, self.cell)?;
        }
        Ok(())
    }

    pub fn event(&mut self, event: Event) -> Result<()> {
        self.content_dirty = true;
        if let Event::Key(key) = event {
            match self.command_keys.key(key, Instant::now()) {
                CommandRoute::Toggle => {
                    self.tab_keys.focus_lost();
                    self.set_explorer(self.explorer.is_none());
                    return Ok(());
                }
                CommandRoute::Consume => return Ok(()),
                CommandRoute::Forward => {}
            }
            if key.kind == KeyEventKind::Press {
                self.swallowed.retain(|code| *code != key.code);
            } else if self.swallowed.contains(&key.code) {
                if key.kind == KeyEventKind::Release {
                    self.swallowed.retain(|code| *code != key.code);
                    return Ok(());
                }
                if self.explorer.is_none() {
                    return Ok(());
                }
            }
            if self.explorer.is_some() && key.kind == KeyEventKind::Press {
                self.swallowed.push(key.code);
            }
            match self.tab_keys.key(key, &self.tree, Instant::now()) {
                Route::Forward => {}
                Route::Consume => return Ok(()),
                Route::Select(action) => {
                    if self.explorer.is_some() {
                        self.set_explorer(false);
                    }
                    self.notice = None;
                    if let Some(key) = self.notice_key.take() {
                        self.timers.remove(&key);
                    }
                    self.capture = Some(Capture::Chrome);
                    if let Err(error) = self.action(action) {
                        self.notify(format!("Tabs: {error:#}"));
                    }
                    return Ok(());
                }
            }
        } else if !matches!(
            event,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Moved,
                ..
            })
        ) {
            self.tab_keys.reset();
            self.command_keys.reset();
        }
        if self.explorer.is_some() {
            match event {
                Event::Key(key) => {
                    if let Some(target) = self.confirm {
                        if key.kind == KeyEventKind::Press {
                            match key.code {
                                KeyCode::Esc => self.set_explorer(false),
                                KeyCode::Enter => {
                                    self.exit_code = 0;
                                    self.remove(target)?;
                                    self.set_explorer(false);
                                }
                                _ => {}
                            }
                        }
                    } else if let Some(command) =
                        self.explorer
                            .as_mut()
                            .unwrap()
                            .key(key, &self.tree, &self.names)
                        && let Err(error) = self.command(command)
                    {
                        self.notify(format!("Sessions: {error:#}"));
                    }
                    self.dirty = true;
                    return Ok(());
                }
                // Never send modal input or drag releases into the child.
                Event::Paste(_) | Event::Mouse(_) => return Ok(()),
                _ => {}
            }
        }
        match event {
            Event::Mouse(mouse) => self.mouse(mouse),
            Event::FocusGained | Event::FocusLost => {
                self.focused = event == Event::FocusGained;
                if !self.focused {
                    self.tab_keys.focus_lost();
                    self.command_keys.reset();
                }
                if let Some(id) = self.tree.active_terminal()
                    && let Err(error) = self
                        .panes
                        .get_mut(&id)
                        .unwrap()
                        .focus(self.focused && self.explorer.is_none())
                {
                    self.notify(format!("Focus: {error:#}"));
                }
                Ok(())
            }
            Event::Resize(_, _) => Ok(()),
            _ => {
                if let Some(id) = self.tree.active_terminal()
                    && let Err(error) = self.panes.get_mut(&id).unwrap().event(event)
                {
                    self.notify(format!("Input: {error:#}"));
                }
                Ok(())
            }
        }
    }

    fn mouse(&mut self, mut mouse: MouseEvent) -> Result<()> {
        let terminal = self.tree.active_terminal();
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.capture = terminal.map(Capture::Terminal);
        }
        let target = match self.capture {
            Some(Capture::Chrome)
                if matches!(mouse.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_)) =>
            {
                None
            }
            Some(Capture::Terminal(id)) => Some(id),
            Some(Capture::Chrome) | None => terminal,
        };
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.capture = None;
        }
        if let Some(id) = target
            && terminal == Some(id)
        {
            mouse.row = mouse.row.min(self.rows - 1);
            mouse.column = mouse.column.min(self.cols - 1);
            if let Err(error) = self.panes.get_mut(&id).unwrap().event(Event::Mouse(mouse)) {
                self.notify(format!("Mouse: {error:#}"));
            }
        }
        Ok(())
    }

    fn set_explorer(&mut self, open: bool) {
        if open {
            let explorer = Explorer::new(&self.tree);
            self.explorer = explorer.selected(&self.tree).map(|_| explorer);
        } else {
            self.explorer = None;
        }
        self.confirm = None;
        self.capture = Some(Capture::Chrome);
        if let Some(id) = self.tree.active_terminal() {
            let _ = self
                .panes
                .get_mut(&id)
                .unwrap()
                .focus(self.focused && !open);
        }
        // Force repaint on dismissal: the pane renderer does not own overlay cells.
        self.dirty = true;
    }

    fn command(&mut self, command: Command) -> Result<()> {
        match command {
            Command::Dismiss => self.set_explorer(false),
            Command::Switch(id) => {
                let previous = self.tree.active_terminal();
                self.tree.activate(id);
                self.changed(previous)?;
                self.set_explorer(false);
            }
            Command::AddSibling(id) => {
                self.action(Action::Add(self.tree.parent(id).unwrap_or(id)))?;
                self.set_explorer(false);
            }
            Command::AddChild(id) => {
                self.action(Action::Add(id))?;
                self.set_explorer(false);
            }
            Command::Close(id) => {
                self.action(Action::Close(id))?;
                if self.confirm.is_none() {
                    self.set_explorer(false);
                }
            }
            Command::Rename(id, name) => {
                if self.tree.contains(id) {
                    if name.is_empty() {
                        self.names.remove(&id);
                    } else {
                        self.names.insert(id, name);
                    }
                }
                self.set_explorer(false);
            }
        }
        Ok(())
    }

    fn action(&mut self, action: Action) -> Result<()> {
        let previous = self.tree.active_terminal();
        match action {
            Action::Add(target) => {
                self.tab_keys.reset();
                let cwd = previous
                    .and_then(|id| self.panes.get(&id)?.session.cwd())
                    .unwrap_or_else(|| self.launch_directory.clone());
                let terminal = self.next_id;
                let group = terminal + 1;
                let mut next = self.tree.clone();
                ensure!(
                    next.add(target, terminal, group),
                    "tab target no longer exists"
                );
                next.activate(terminal);
                let pane = Pane::new(
                    &self.shell,
                    &cwd,
                    terminal,
                    (self.cols, self.rows, self.cell),
                    self.bus.clone(),
                    &mut self.watch,
                )?;
                self.next_id += 2;
                self.panes.insert(terminal, pane);
                self.tree = next;
            }
            Action::Select { group, child } => {
                self.tree.select(group, child);
            }
            Action::Close(target) => {
                self.exit_code = 0;
                if self
                    .tree
                    .terminals_in(target)
                    .iter()
                    .any(|id| self.panes[id].session.has_foreground_job())
                {
                    self.confirm = Some(target);
                    self.dirty = true;
                    return Ok(());
                }
                return self.remove(target);
            }
        }
        self.changed(previous)
    }

    fn changed(&mut self, previous: Option<Id>) -> Result<()> {
        let active = self.tree.active_terminal();
        if previous != active {
            if let Some(pane) = previous.and_then(|id| self.panes.get_mut(&id)) {
                let _ = pane.focus(false);
            }
            if let Some(pane) = active.and_then(|id| self.panes.get_mut(&id))
                && let Err(error) = pane.focus(self.focused && self.explorer.is_none())
            {
                self.notify(format!("Focus: {error:#}"));
            }
        }
        self.layout_panes()?;
        self.dirty = true;
        Ok(())
    }

    fn remove(&mut self, target: Id) -> Result<()> {
        self.tab_keys.reset();
        self.command_keys.reset();
        let previous = self.tree.active_terminal();
        let removed = self.tree.remove(target);
        let mut sessions = Vec::new();
        for id in removed {
            if let Some(key) = self.deadlines.remove(&id) {
                self.timers.remove(&key);
            }
            if let Some(pane) = self.panes.remove(&id) {
                let _ = self.watch.unwatch(pane.session.pid());
                sessions.push(pane.into_session());
            }
        }
        if !sessions.is_empty() {
            self.cleanup.spawn_blocking(move || drop(sessions));
        }
        if self.confirm.is_some_and(|id| !self.tree.contains(id)) {
            self.confirm = None;
        }
        self.names.retain(|id, _| self.tree.contains(*id));
        self.capture = Some(Capture::Chrome);
        self.changed(previous)
    }

    fn notify(&mut self, message: String) {
        self.notice = Some((message, Instant::now()));
        if let Some(key) = self.notice_key {
            self.timers.reset(&key, Duration::from_secs(5));
        } else {
            self.notice_key = Some(self.timers.insert(Timeout::Notice, Duration::from_secs(5)));
        }
        self.confirm = None;
        self.dirty = true;
    }

    pub fn needs_render(&self) -> bool {
        self.dirty || self.content_dirty
    }

    pub fn draw(&mut self, output: &mut impl Write) -> Result<bool> {
        if !self.needs_render() {
            return Ok(false);
        }
        self.frame.clear();
        if self.dirty || self.explorer.is_some() {
            self.frame.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        }
        if self.dirty {
            self.frame.extend_from_slice(b"\x1b[0m\x1b[2J");
        }
        if let Some(id) = self.tree.active_terminal() {
            self.panes
                .get_mut(&id)
                .unwrap()
                .draw(&mut self.frame, 0, self.dirty)?;
        }
        // Repaint over each incremental terminal frame, not just UI changes.
        if let Some(explorer) = &self.explorer {
            explorer.draw(
                &mut self.frame,
                self.cols,
                self.rows,
                &self.tree,
                &self.names,
                self.confirm.is_some(),
            )?;
        }
        if !self.frame.is_empty() {
            self.frame.extend_from_slice(b"\x1b[?2026l");
            output.write_all(&self.frame)?;
            output.flush()?;
        }
        self.dirty = false;
        self.content_dirty = false;
        Ok(!self.frame.is_empty())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.panes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn a_clean_render_does_not_count_as_a_presented_frame() {
        let (bus, _data, _control) = crate::bus::channel();
        let mut app = App::new(PathBuf::from("/bin/sh"), 12, 4, (0, 0), bus).unwrap();
        let mut bytes = Vec::new();
        assert!(app.draw(&mut bytes).unwrap());
        bytes.clear();
        assert!(!app.draw(&mut bytes).unwrap());
        assert!(bytes.is_empty());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn overlay_composes_before_frame_commit_and_dismissal_restores_terminal() {
        let (bus, _data, _control) = crate::bus::channel();
        let mut app = App::new(PathBuf::from("/bin/sh"), 80, 24, (0, 0), bus).unwrap();
        app.panes
            .get_mut(&1)
            .unwrap()
            .output(b"\x1b[2J\x1b[HBASE")
            .unwrap();
        let mut host = libghostty_vt::Terminal::new(80, 24).unwrap();
        host.vt_write(b"\x1b[?7l");
        for open in [false, true, true, false] {
            if open != app.explorer.is_some() {
                app.set_explorer(open);
            }
            app.panes
                .get_mut(&1)
                .unwrap()
                .output(b"\x1b[12;1Hlive output")
                .unwrap();
            app.content_dirty = true;
            let mut bytes = Vec::new();
            assert!(app.draw(&mut bytes).unwrap());
            let commit = b"\x1b[?2026l";
            assert_eq!(
                bytes
                    .windows(commit.len())
                    .filter(|part| *part == commit)
                    .count(),
                1
            );
            assert!(bytes.ends_with(commit));
            assert_eq!(String::from_utf8_lossy(&bytes).contains('›'), open);
            host.vt_write(&bytes);
        }
        use libghostty_vt::terminal::{Point, PointCoordinate};
        for (x, ch) in "live output".chars().enumerate() {
            let cell = host
                .grid_ref(Point::Active(PointCoordinate { x: x as u16, y: 11 }))
                .unwrap()
                .cell()
                .unwrap();
            assert_eq!(cell.codepoint().unwrap(), ch as u32);
        }
        app.shutdown().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explorer_browsing_and_cancel_preserve_selection() {
        use crossterm::event::{KeyEvent, KeyModifiers};
        let (bus, _data, _control) = crate::bus::channel();
        let mut app = App::new(PathBuf::from("/bin/sh"), 80, 24, (0, 0), bus).unwrap();
        app.command(Command::AddSibling(1)).unwrap();
        let selected = app.tree.active_terminal();
        app.set_explorer(true);
        app.event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)))
            .unwrap();
        assert_eq!(app.tree.active_terminal(), selected);
        app.event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)))
            .unwrap();
        assert!(app.explorer.is_none());
        assert_eq!(app.tree.active_terminal(), selected);
        assert!(app.dirty);
        // Even the release of a closing action is owned by the modal UI.
        app.event(Event::Key(KeyEvent::new_with_kind(
            KeyCode::Esc,
            KeyModifiers::NONE,
            KeyEventKind::Release,
        )))
        .unwrap();
        assert!(!app.swallowed.contains(&KeyCode::Esc));
        app.shutdown().await;
    }
}
