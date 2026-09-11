use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::PathBuf,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Result, bail, ensure};
use crossterm::event::{Event, MouseButton, MouseEvent, MouseEventKind};

use crate::{
    pane::Pane,
    tree::{Action, Id, Layout, Tree},
    wake::Wake,
};

pub struct App {
    tree: Tree,
    panes: BTreeMap<Id, Pane>,
    shell: PathBuf,
    launch_directory: PathBuf,
    next_id: Id,
    cols: u16,
    rows: u16,
    cell: (u16, u16),
    dirty: bool,
    content_dirty: bool,
    wake: Wake,
    focused: bool,
    capture: Option<Capture>,
    confirm: Option<Id>,
    notice: Option<(String, Instant)>,
    cleanup: Vec<JoinHandle<()>>,
    rotation: usize,
    exit_code: u8,
}

#[derive(Clone, Copy)]
enum Capture {
    Terminal(Id),
    Chrome,
}

impl App {
    pub fn new(shell: PathBuf, cols: u16, rows: u16, cell: (u16, u16), wake: Wake) -> Result<Self> {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let launch_directory = std::env::current_dir()?;
        let tree = Tree::new(1);
        let layout = tree.layout(cols, rows);
        let pane = Pane::new(
            &shell,
            &launch_directory,
            cols,
            layout.rows,
            cell,
            wake.clone(),
        )?;
        Ok(Self {
            tree,
            panes: BTreeMap::from([(1, pane)]),
            shell,
            launch_directory,
            next_id: 2,
            cols,
            rows,
            cell,
            dirty: true,
            content_dirty: true,
            wake,
            focused: true,
            capture: None,
            confirm: None,
            notice: None,
            cleanup: Vec::new(),
            rotation: 0,
            exit_code: 0,
        })
    }

    pub fn tick(&mut self) -> Result<(Option<u8>, bool)> {
        self.cleanup.retain(|job| !job.is_finished());
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5))
        {
            self.notice = None;
            self.dirty = true;
        }
        let ids: Vec<_> = self.panes.keys().copied().collect();
        let mut budget = 64;
        let started = Instant::now();
        let mut finished = BTreeMap::new();
        let active = self.tree.active_terminal();
        let more_work = loop {
            let mut consumed = false;
            for n in 0..ids.len() {
                let id = ids[(self.rotation + n) % ids.len()];
                if finished.contains_key(&id) {
                    continue;
                }
                let can_read = budget > 0 && started.elapsed() < Duration::from_millis(2);
                match self.panes.get_mut(&id).unwrap().poll(can_read) {
                    Ok(progress) => {
                        if progress.consumed {
                            budget -= 1;
                            consumed = true;
                            self.content_dirty |= Some(id) == active;
                        }
                        if let Some(code) = progress.exit {
                            finished.insert(id, (code, None));
                        }
                    }
                    Err(error) => {
                        finished.insert(id, (1, Some(format!("Shell {id}: {error:#}"))));
                    }
                }
            }
            if !consumed {
                break false;
            }
            if budget == 0 || started.elapsed() >= Duration::from_millis(2) {
                break true;
            }
        };
        self.rotation = if ids.is_empty() {
            0
        } else {
            (self.rotation + 1) % ids.len()
        };
        for (id, (code, error)) in finished {
            self.exit_code = code;
            self.remove(id)?;
            if let Some(message) = error {
                if self.tree.root.is_none() {
                    bail!(message);
                }
                self.notify(message);
            }
        }
        Ok((
            self.tree.root.is_none().then_some(self.exit_code),
            more_work,
        ))
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell: (u16, u16)) -> Result<()> {
        self.cols = cols.max(1);
        self.rows = rows.max(1);
        self.cell = cell;
        self.capture = None;
        self.layout_panes()?;
        self.dirty = true;
        Ok(())
    }

    fn layout_panes(&mut self) -> Result<()> {
        for (id, depth) in self.tree.terminal_depths() {
            let top = depth.min(usize::from(self.rows.saturating_sub(1))) as u16;
            self.panes
                .get_mut(&id)
                .unwrap()
                .resize(self.cols, self.rows - top, self.cell)?;
        }
        Ok(())
    }

    pub fn event(&mut self, event: Event) -> Result<()> {
        self.content_dirty = true;
        match event {
            Event::Mouse(mouse) => self.mouse(mouse),
            Event::FocusGained | Event::FocusLost => {
                self.focused = event == Event::FocusGained;
                if let Some(id) = self.tree.active_terminal()
                    && let Err(error) = self.panes.get_mut(&id).unwrap().focus(self.focused)
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
        let layout = self.tree.layout(self.cols, self.rows);
        if matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.capture = Some(if mouse.row < layout.top {
                Capture::Chrome
            } else {
                let Some(id) = layout.terminal else {
                    return Ok(());
                };
                Capture::Terminal(id)
            });
            if mouse.kind == MouseEventKind::Down(MouseButton::Left) && mouse.row < layout.top {
                if mouse.row == 0 && self.confirm.is_some() {
                    if let Some(target) = self.confirm
                        && confirm_yes(self.cols).is_some_and(|x| mouse.column >= x)
                    {
                        self.exit_code = 0;
                        self.remove(target)?;
                    } else {
                        self.confirm = None;
                        self.dirty = true;
                    }
                    return Ok(());
                }
                if mouse.row == 0 && self.notice.take().is_some() {
                    self.dirty = true;
                    return Ok(());
                }
                if let Some(action) = layout.hit(mouse.column, mouse.row) {
                    self.confirm = None;
                    if let Err(error) = self.action(action) {
                        self.notify(format!("Tabs: {error:#}"));
                    }
                }
                return Ok(());
            }
        }
        let target = match self.capture {
            Some(Capture::Chrome) => None,
            Some(Capture::Terminal(id)) => Some(id),
            None if mouse.row >= layout.top => layout.terminal,
            None => None,
        };
        if matches!(mouse.kind, MouseEventKind::Up(_)) {
            self.capture = None;
        }
        if let Some(id) = target
            && layout.terminal == Some(id)
        {
            mouse.row = mouse
                .row
                .saturating_sub(layout.top)
                .min(layout.rows.saturating_sub(1));
            mouse.column = mouse.column.min(self.cols - 1);
            if let Err(error) = self.panes.get_mut(&id).unwrap().event(Event::Mouse(mouse)) {
                self.notify(format!("Mouse: {error:#}"));
            }
        }
        Ok(())
    }

    fn action(&mut self, action: Action) -> Result<()> {
        let previous = self.tree.active_terminal();
        match action {
            Action::Add(target) => {
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
                let layout = next.layout(self.cols, self.rows);
                let pane = Pane::new(
                    &self.shell,
                    &cwd,
                    self.cols,
                    layout.rows,
                    self.cell,
                    self.wake.clone(),
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
                && let Err(error) = pane.focus(self.focused)
            {
                self.notify(format!("Focus: {error:#}"));
            }
        }
        self.layout_panes()?;
        self.dirty = true;
        Ok(())
    }

    fn remove(&mut self, target: Id) -> Result<()> {
        let previous = self.tree.active_terminal();
        let removed = self.tree.remove(target);
        let sessions: Vec<_> = removed
            .into_iter()
            .filter_map(|id| self.panes.remove(&id))
            .map(Pane::into_session)
            .collect();
        if !sessions.is_empty() {
            self.cleanup.push(
                thread::Builder::new()
                    .name("pane-cleanup".into())
                    .spawn(move || drop(sessions))?,
            );
        }
        self.confirm = None;
        self.capture = Some(Capture::Chrome);
        self.changed(previous)
    }

    fn notify(&mut self, message: String) {
        self.notice = Some((message, Instant::now()));
        self.confirm = None;
        self.dirty = true;
    }

    pub fn needs_render(&self) -> bool {
        self.dirty || self.content_dirty
    }

    pub fn draw(&mut self, output: &mut impl Write) -> Result<bool> {
        let layout = self.tree.layout(self.cols, self.rows);
        let mut frame = Vec::new();
        if self.dirty {
            frame.extend_from_slice(b"\x1b[?2026h\x1b[?25l\x1b[0m\x1b[2J");
            draw_bars(&layout, &mut frame)?;
            if !layout.bars.is_empty() {
                if self.confirm.is_some() {
                    let text = if confirm_yes(self.cols).is_some() {
                        format!(
                            "{}[Cancel] [Close]",
                            fit("Close running programs?", usize::from(self.cols - 16))
                        )
                    } else {
                        "Enlarge to confirm; click to cancel".to_owned()
                    };
                    banner(&mut frame, self.cols, &text)?;
                } else if let Some((message, _)) = &self.notice {
                    banner(&mut frame, self.cols, message)?;
                }
            }
        }
        if let Some(id) = layout.terminal {
            self.panes
                .get_mut(&id)
                .unwrap()
                .draw(&mut frame, layout.top, self.dirty)?;
        }
        if !frame.is_empty() {
            frame.extend_from_slice(b"\x1b[?2026l");
            output.write_all(&frame)?;
            output.flush()?;
        }
        self.dirty = false;
        self.content_dirty = false;
        Ok(!frame.is_empty())
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.panes.clear();
        for job in self.cleanup.drain(..) {
            let _ = job.join();
        }
    }
}

fn confirm_yes(cols: u16) -> Option<u16> {
    (cols >= 16).then(|| cols - 7)
}

fn banner(output: &mut impl Write, cols: u16, text: &str) -> io::Result<()> {
    write!(
        output,
        "\x1b[1;1H\x1b[0;30;43m{}\x1b[0m",
        fit(text, usize::from(cols))
    )
}

fn draw_bars(layout: &Layout, output: &mut impl Write) -> io::Result<()> {
    for bar in &layout.bars {
        write!(
            output,
            "\x1b[{};1H\x1b[0;37;48;5;238m{}",
            bar.row + 1,
            " ".repeat(usize::from(layout.cols))
        )?;
        for tab in &bar.tabs {
            if tab.width == 0 {
                continue;
            }
            let color = if tab.active {
                "0;30;47"
            } else {
                "0;37;48;5;238"
            };
            let label_width = usize::from(tab.width) - if tab.close_x.is_some() { 3 } else { 0 };
            write!(
                output,
                "\x1b[{};{}H\x1b[{color}m{}",
                bar.row + 1,
                tab.x + 1,
                fit(&format!(" {}", tab.label), label_width)
            )?;
            if tab.close_x.is_some() {
                output.write_all(b"[x]")?;
            }
        }
        write!(
            output,
            "\x1b[{};{}H\x1b[0;30;46m{}\x1b[0m",
            bar.row + 1,
            bar.plus_x + 1,
            if bar.plus_width == 3 { "[+]" } else { "+" }
        )?;
    }
    Ok(())
}

fn fit(text: &str, width: usize) -> String {
    let mut text: String = text
        .chars()
        .take(width)
        .map(|ch| {
            if ch.is_ascii() && !ch.is_control() {
                ch
            } else {
                '?'
            }
        })
        .collect();
    text.extend(std::iter::repeat_n(' ', width.saturating_sub(text.len())));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::{
        Terminal, TerminalOptions,
        terminal::{Point, PointCoordinate},
    };

    #[test]
    fn a_clean_render_does_not_count_as_a_presented_frame() {
        let mut app = App::new(PathBuf::from("/bin/sh"), 12, 4, (0, 0), Wake::new()).unwrap();
        let mut bytes = Vec::new();
        assert!(app.draw(&mut bytes).unwrap());
        bytes.clear();
        assert!(!app.draw(&mut bytes).unwrap());
        assert!(bytes.is_empty());
    }

    #[test]
    fn painted_controls_match_layout_hit_targets() {
        let mut tree = Tree::new(1);
        tree.add(1, 2, 3);
        tree.add(2, 4, 5);
        for cols in [1, 2, 3, 7, 17, 80] {
            let layout = tree.layout(cols, 24);
            let mut bytes = Vec::new();
            draw_bars(&layout, &mut bytes).unwrap();
            let mut host = Terminal::new(TerminalOptions {
                cols,
                rows: 24,
                max_scrollback: 0,
            })
            .unwrap();
            host.vt_write(b"\x1b[?7l");
            host.vt_write(&bytes);
            for bar in &layout.bars {
                let x = bar.plus_x + u16::from(bar.plus_width == 3);
                let cell = host
                    .grid_ref(Point::Active(PointCoordinate {
                        x,
                        y: u32::from(bar.row),
                    }))
                    .unwrap()
                    .cell()
                    .unwrap();
                assert_eq!(cell.codepoint().unwrap(), '+' as u32);
                assert!(matches!(layout.hit(x, bar.row), Some(Action::Add(id)) if id == bar.owner));
                for tab in &bar.tabs {
                    if let Some(x) = tab.close_x {
                        let cell = host
                            .grid_ref(Point::Active(PointCoordinate {
                                x: x + 1,
                                y: u32::from(bar.row),
                            }))
                            .unwrap()
                            .cell()
                            .unwrap();
                        assert_eq!(cell.codepoint().unwrap(), 'x' as u32);
                        assert!(
                            matches!(layout.hit(x + 1, bar.row), Some(Action::Close(id)) if id == tab.id)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn banner_text_is_bounded_and_cannot_inject_terminal_sequences() {
        assert_eq!(fit("hello", 3), "hel");
        assert_eq!(fit("hi", 5), "hi   ");
        assert_eq!(fit("\x1b[2J\n", 6), "?[2J? ");
        assert_eq!(confirm_yes(80), Some(73));
        assert_eq!(confirm_yes(15), None);
    }
}
