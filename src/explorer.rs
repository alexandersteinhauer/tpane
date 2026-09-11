//! Pure session-explorer navigation and an opaque, bounded terminal overlay.
//!
//! Commands describe intent only; the caller owns tree/PTY mutations and close
//! confirmation. All groups initially expand, including inactive branches.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::queue;
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::tree::{Id, Node, Tree};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Dismiss,
    Switch(Id),
    AddSibling(Id),
    AddChild(Id),
    Close(Id),
    Rename(Id, String),
}

#[derive(Clone, Debug)]
pub struct Explorer {
    selection: Option<Id>,
    collapsed: BTreeSet<Id>,
    rename: Option<(Id, String)>,
}

struct Entry<'a> {
    node: &'a Node,
    parent: Option<Id>,
    depth: usize,
    number: String,
}

impl Explorer {
    pub fn new(tree: &Tree) -> Self {
        Self {
            selection: tree.active_terminal(),
            collapsed: BTreeSet::new(),
            rename: None,
        }
    }

    fn entries<'a>(&self, tree: &'a Tree) -> Vec<Entry<'a>> {
        fn visit<'a>(
            node: &'a Node,
            parent: Option<Id>,
            depth: usize,
            number: String,
            collapsed: &BTreeSet<Id>,
            out: &mut Vec<Entry<'a>>,
        ) {
            out.push(Entry {
                node,
                parent,
                depth,
                number: number.clone(),
            });
            if !collapsed.contains(&node.id())
                && let Node::Tabs { children, .. } = node
            {
                for (index, child) in children.iter().enumerate() {
                    let digit = index + 1;
                    let number = if number.is_empty() {
                        digit.to_string()
                    } else {
                        format!("{number}.{digit}")
                    };
                    visit(child, Some(node.id()), depth + 1, number, collapsed, out);
                }
            }
        }
        let mut entries = Vec::new();
        if let Some(root) = &tree.root {
            let number = if matches!(root, Node::Terminal(_)) {
                "1"
            } else {
                ""
            };
            visit(root, None, 0, number.into(), &self.collapsed, &mut entries);
        }
        entries
    }

    fn index(&self, entries: &[Entry<'_>], tree: &Tree) -> Option<usize> {
        entries
            .iter()
            .position(|entry| Some(entry.node.id()) == self.selection)
            .or_else(|| {
                entries
                    .iter()
                    .position(|entry| Some(entry.node.id()) == tree.active_terminal())
            })
            .or_else(|| (!entries.is_empty()).then_some(0))
    }

    /// Resolve against the current tree, tolerating removal of a selected node.
    pub fn selected(&self, tree: &Tree) -> Option<Id> {
        let entries = self.entries(tree);
        self.index(&entries, tree)
            .map(|index| entries[index].node.id())
    }

    pub fn key(
        &mut self,
        key: KeyEvent,
        tree: &Tree,
        names: &BTreeMap<Id, String>,
    ) -> Option<Command> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        let press = key.kind == KeyEventKind::Press;
        // Do not turn terminal/application shortcuts into explorer actions.
        let plain = (key.modifiers & !KeyModifiers::SHIFT).is_empty();
        if !plain {
            return None;
        }
        if let Some((_, text)) = &mut self.rename {
            match key.code {
                KeyCode::Esc if press => self.rename = None,
                KeyCode::Enter if press => {
                    let (id, text) = self.rename.take().unwrap();
                    if self.entries(tree).iter().any(|entry| entry.node.id() == id) {
                        return Some(Command::Rename(id, text));
                    }
                }
                KeyCode::Backspace => {
                    if let Some((start, _)) = text.grapheme_indices(true).next_back() {
                        text.truncate(start);
                    }
                }
                KeyCode::Char(c) if !c.is_control() => text.push(c),
                _ => {}
            }
            return None;
        }
        if key.code == KeyCode::Esc && press {
            return Some(Command::Dismiss);
        }
        let entries = self.entries(tree);
        let index = self.index(&entries, tree)?;
        let entry = &entries[index];
        let id = entry.node.id();
        self.selection = Some(id);
        match key.code {
            KeyCode::Up => self.selection = Some(entries[index.saturating_sub(1)].node.id()),
            KeyCode::Down => {
                self.selection = Some(entries[(index + 1).min(entries.len() - 1)].node.id())
            }
            KeyCode::Home => self.selection = Some(entries[0].node.id()),
            KeyCode::End => self.selection = Some(entries[entries.len() - 1].node.id()),
            KeyCode::Left => {
                if matches!(entry.node, Node::Tabs { .. }) && !self.collapsed.contains(&id) {
                    self.collapsed.insert(id);
                } else if let Some(parent) = entry.parent {
                    self.selection = Some(parent);
                }
            }
            KeyCode::Right => {
                if let Node::Tabs { children, .. } = entry.node
                    && !self.collapsed.remove(&id)
                    && let Some(child) = children.first()
                {
                    self.selection = Some(child.id());
                }
            }
            KeyCode::Enter if press => return Some(Command::Switch(entry.node.active_terminal())),
            KeyCode::Char('n') if press && key.modifiers.contains(KeyModifiers::SHIFT) => {
                return Some(Command::AddChild(id));
            }
            KeyCode::Char('n') if press => return Some(Command::AddSibling(id)),
            KeyCode::Char('N') if press => return Some(Command::AddChild(id)),
            KeyCode::Char('d') if press => return Some(Command::Close(id)),
            KeyCode::Char('r') if press => {
                self.rename = Some((id, names.get(&id).cloned().unwrap_or_default()));
            }
            _ => {}
        }
        None
    }

    pub fn draw(
        &self,
        output: &mut impl Write,
        cols: u16,
        rows: u16,
        tree: &Tree,
        names: &BTreeMap<Id, String>,
        confirm: bool,
    ) -> io::Result<()> {
        queue!(output, Hide)?;
        if cols == 0 || rows == 0 {
            return Ok(());
        }
        let entries = self.entries(tree);
        let selected = self.index(&entries, tree);
        let width = cols.min(if confirm { 52 } else { 78 });
        let desired_height = if confirm {
            5
        } else {
            entries.len().saturating_add(4).clamp(5, 24) as u16
        };
        let height = rows.min(desired_height);
        let left = (cols - width) / 2;
        let top = (rows - height) / 2;
        // Small terminals give all available cells to content instead of borders.
        let bordered = width >= 3 && height >= 3;
        let inset = usize::from(bordered);
        let inner_width = usize::from(width) - 2 * inset;
        let inner_height = usize::from(height) - 2 * inset;
        let mut lines = vec![(String::new(), false); inner_height];
        if confirm {
            lines[(inner_height - 1) / 2].0 = "Close running programs?".into();
            if inner_height >= 2 {
                lines[inner_height / 2 + usize::from(inner_height > 2)].0 =
                    "↵ close · esc cancel".into();
            }
        } else {
            let header = usize::from(inner_height >= 3);
            let footer = usize::from(inner_height >= 2);
            let capacity = inner_height - header - footer;
            if header != 0 {
                lines[0].0 = " Sessions".into();
            }
            let start = window_start(selected.unwrap_or(0), entries.len(), capacity);
            for (line, index) in (start..entries.len()).take(capacity).enumerate() {
                let entry = &entries[index];
                let active = if Some(entry.node.id()) == tree.active_terminal() {
                    " ●"
                } else {
                    ""
                };
                let branch = match entry.node {
                    Node::Tabs { .. } if self.collapsed.contains(&entry.node.id()) => '▸',
                    Node::Tabs { .. } => '▾',
                    Node::Terminal(_) => ' ',
                };
                // Cap indentation so pathological depth cannot allocate a huge line.
                let indent = " ".repeat(entry.depth.saturating_mul(2).min(inner_width / 3));
                lines[header + line] = (
                    format!(
                        " {} {indent}{branch} {}{active}",
                        if selected == Some(index) { '›' } else { ' ' },
                        label(entry, names)
                    ),
                    selected == Some(index),
                );
            }
            if entries.is_empty() && capacity > 0 {
                lines[header].0 = "No sessions".into();
            }
            if footer != 0 {
                lines[inner_height - 1].0 =
                    " ↑↓ move  ←→ fold  ↵ open  n/N new  r rename  d close  esc back".into();
            }
            if let Some((_, text)) = &self.rename {
                // Tail keeps the edit position visible without splitting graphemes.
                if header != 0 {
                    lines[0].0 = " Sessions · name · ↵ save · esc cancel".into();
                }
                let prompt = " Name: ";
                let room = inner_width.saturating_sub(prompt.width() + 1);
                lines[inner_height - 1].0 = format!("{prompt}{}▏", tail(text, room));
            }
        }
        queue!(output, SetAttribute(Attribute::Reset))?;
        for row in 0..usize::from(height) {
            queue!(
                output,
                MoveTo(left, top + row as u16),
                SetForegroundColor(Color::DarkGrey),
                SetBackgroundColor(Color::Reset)
            )?;
            if bordered && (row == 0 || row + 1 == usize::from(height)) {
                let (left, right) = if row == 0 {
                    ('╭', '╮')
                } else {
                    ('╰', '╯')
                };
                queue!(
                    output,
                    Print(format!("{left}{}{right}", "─".repeat(inner_width)))
                )?;
                continue;
            }
            let (text, highlight) = &lines[row - inset];
            if bordered {
                queue!(output, Print("│"))?;
            }
            queue!(output, SetForegroundColor(Color::Grey))?;
            if *highlight {
                queue!(
                    output,
                    SetForegroundColor(Color::White),
                    SetBackgroundColor(Color::DarkGrey)
                )?;
            }
            queue!(
                output,
                Print(fit(text, inner_width)),
                SetForegroundColor(Color::DarkGrey),
                SetBackgroundColor(Color::Reset)
            )?;
            if bordered {
                queue!(output, Print("│"))?;
            }
        }
        queue!(output, ResetColor, SetAttribute(Attribute::Reset))
    }
}

fn label(entry: &Entry<'_>, names: &BTreeMap<Id, String>) -> String {
    match (entry.number.as_str(), names.get(&entry.node.id())) {
        ("", Some(name)) => name.clone(),
        ("", None) => "All sessions".into(),
        (number, Some(name)) => format!("{number} {name}"),
        (number, None) => number.into(),
    }
}

fn window_start(selected: usize, count: usize, capacity: usize) -> usize {
    selected
        .saturating_sub(capacity / 2)
        .min(count.saturating_sub(capacity))
}

// Preserve Unicode labels, but never allow controls or bidi formatting to
// escape/reorder the dialog. Clip at grapheme boundaries using terminal cells.
fn clean(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                '�'
            } else {
                c
            }
        })
        .collect()
}

fn fit(text: &str, width: usize) -> String {
    let clean = clean(text);
    let mut result = String::new();
    let mut used = 0;
    for grapheme in clean.graphemes(true) {
        let cells = grapheme.width();
        if used + cells > width {
            break;
        }
        // A leading combining mark must not modify the preceding border cell.
        if cells == 0 && result.is_empty() {
            continue;
        }
        result.push_str(grapheme);
        used += cells;
    }
    result.extend(std::iter::repeat_n(' ', width - used));
    result
}

fn tail(text: &str, width: usize) -> String {
    let clean = clean(text);
    let mut used = 0;
    let mut start = clean.len();
    for (offset, grapheme) in clean.grapheme_indices(true).rev() {
        let cells = grapheme.width();
        if used + cells > width {
            break;
        }
        used += cells;
        start = offset;
    }
    clean[start..].into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested() -> Tree {
        let mut tree = Tree::new(1);
        tree.add(1, 2, 10);
        tree.add(2, 3, 20);
        tree
    }

    fn key(explorer: &mut Explorer, tree: &Tree, code: KeyCode) -> Option<Command> {
        explorer.key(
            KeyEvent::new(code, KeyModifiers::NONE),
            tree,
            &BTreeMap::new(),
        )
    }

    #[test]
    fn starts_at_active_and_visits_entire_tree() {
        let mut tree = nested();
        tree.select(10, 1);
        let mut explorer = Explorer::new(&tree);
        assert_eq!(explorer.selected(&tree), Some(1));
        assert_eq!(
            explorer
                .entries(&tree)
                .iter()
                .map(|e| e.node.id())
                .collect::<Vec<_>>(),
            vec![10, 1, 20, 2, 3]
        );
        key(&mut explorer, &tree, KeyCode::Home);
        for id in [10, 1, 20, 2, 3, 3] {
            assert_eq!(explorer.selected(&tree), Some(id));
            key(&mut explorer, &tree, KeyCode::Down);
        }
        key(&mut explorer, &tree, KeyCode::Home);
        key(&mut explorer, &tree, KeyCode::Up);
        assert_eq!(explorer.selected(&tree), Some(10));
    }

    #[test]
    fn collapse_expand_and_parent_child_navigation() {
        let tree = nested();
        let mut explorer = Explorer::new(&tree);
        key(&mut explorer, &tree, KeyCode::Left);
        assert_eq!(explorer.selected(&tree), Some(20));
        key(&mut explorer, &tree, KeyCode::Left);
        assert_eq!(explorer.entries(&tree).len(), 3);
        key(&mut explorer, &tree, KeyCode::Right);
        assert_eq!(explorer.entries(&tree).len(), 5);
        assert_eq!(explorer.selected(&tree), Some(20));
        key(&mut explorer, &tree, KeyCode::Right);
        assert_eq!(explorer.selected(&tree), Some(2));
        key(&mut explorer, &tree, KeyCode::Home);
        key(&mut explorer, &tree, KeyCode::Left);
        assert_eq!(explorer.entries(&tree).len(), 1);
        key(&mut explorer, &tree, KeyCode::Left);
        assert_eq!(explorer.selected(&tree), Some(10));
    }

    #[test]
    fn commands_are_intents_and_groups_switch_remembered_leaf() {
        let mut tree = nested();
        tree.select(10, 1);
        let before = tree.clone();
        let mut explorer = Explorer::new(&tree);
        key(&mut explorer, &tree, KeyCode::Down);
        assert_eq!(explorer.selected(&tree), Some(20));
        for (code, command) in [
            (KeyCode::Enter, Command::Switch(3)),
            (KeyCode::Char('n'), Command::AddSibling(20)),
            (KeyCode::Char('N'), Command::AddChild(20)),
            (KeyCode::Char('d'), Command::Close(20)),
            (KeyCode::Esc, Command::Dismiss),
        ] {
            assert_eq!(key(&mut explorer, &tree, code), Some(command));
        }
        assert_eq!(tree, before);
    }

    #[test]
    fn releases_and_modified_shortcuts_are_ignored_and_actions_do_not_repeat() {
        let tree = nested();
        let mut explorer = Explorer::new(&tree);
        let names = BTreeMap::new();
        for kind in [KeyEventKind::Release, KeyEventKind::Repeat] {
            for code in [
                KeyCode::Enter,
                KeyCode::Char('d'),
                KeyCode::Char('n'),
                KeyCode::Char('N'),
                KeyCode::Char('r'),
                KeyCode::Esc,
            ] {
                assert_eq!(
                    explorer.key(
                        KeyEvent::new_with_kind(code, KeyModifiers::NONE, kind),
                        &tree,
                        &names
                    ),
                    None
                );
                assert!(explorer.rename.is_none());
            }
        }
        explorer.key(
            KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Release),
            &tree,
            &names,
        );
        assert_eq!(explorer.selected(&tree), Some(3));
        explorer.key(
            KeyEvent::new_with_kind(KeyCode::Up, KeyModifiers::NONE, KeyEventKind::Repeat),
            &tree,
            &names,
        );
        assert_eq!(explorer.selected(&tree), Some(2));
        assert_eq!(
            explorer.key(
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
                &tree,
                &names
            ),
            None
        );
        assert_eq!(
            explorer.key(
                KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT),
                &tree,
                &names
            ),
            Some(Command::AddChild(2))
        );
    }

    #[test]
    fn rename_is_modal_and_commits_only_on_pressed_enter() {
        let tree = nested();
        let mut explorer = Explorer::new(&tree);
        let names = BTreeMap::from([(3, "work".into())]);
        explorer.key(
            KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE),
            &tree,
            &names,
        );
        key(&mut explorer, &tree, KeyCode::Backspace);
        key(&mut explorer, &tree, KeyCode::Char('é'));
        key(&mut explorer, &tree, KeyCode::Char('d'));
        key(&mut explorer, &tree, KeyCode::Up);
        assert_eq!(explorer.selected(&tree), Some(3));
        assert_eq!(
            explorer.key(
                KeyEvent::new_with_kind(KeyCode::Enter, KeyModifiers::NONE, KeyEventKind::Repeat),
                &tree,
                &names
            ),
            None
        );
        assert_eq!(
            key(&mut explorer, &tree, KeyCode::Enter),
            Some(Command::Rename(3, "woréd".into()))
        );
        key(&mut explorer, &tree, KeyCode::Char('r'));
        assert_eq!(key(&mut explorer, &tree, KeyCode::Esc), None);
        assert!(explorer.rename.is_none());
        assert_eq!(
            key(&mut explorer, &tree, KeyCode::Esc),
            Some(Command::Dismiss)
        );
    }

    #[test]
    fn selection_recovers_after_removal_and_empty_tree_is_safe() {
        let mut tree = nested();
        let mut explorer = Explorer::new(&tree);
        key(&mut explorer, &tree, KeyCode::Char('r'));
        tree.remove(3);
        assert_eq!(explorer.selected(&tree), Some(2));
        assert_eq!(key(&mut explorer, &tree, KeyCode::Enter), None);
        tree.remove(10);
        assert_eq!(explorer.selected(&tree), None);
        for code in [
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Right,
            KeyCode::Enter,
            KeyCode::Char('n'),
        ] {
            assert_eq!(key(&mut explorer, &tree, code), None);
        }
        assert_eq!(
            key(&mut explorer, &tree, KeyCode::Esc),
            Some(Command::Dismiss)
        );
    }

    #[test]
    fn labels_use_positions_not_internal_ids_and_preserve_explicit_names() {
        let mut tree = nested();
        for id in 4..=11 {
            tree.add(10, id, 999);
        }
        let explorer = Explorer::new(&tree);
        let entries = explorer.entries(&tree);
        let names = BTreeMap::from([
            (10, "renamed root".into()),
            (20, "Projects".into()),
            (2, "shell".into()),
        ]);
        assert_eq!(label(&entries[0], &names), "renamed root");
        assert_eq!(label(&entries[2], &names), "2 Projects");
        assert_eq!(label(&entries[3], &names), "2.1 shell");
        assert_eq!(label(&entries[2], &BTreeMap::new()), "2");
        assert_eq!(entries[3].number, "2.1");
        assert_eq!(entries.last().unwrap().number, "10");
        assert_eq!(
            label(
                &Explorer::new(&Tree::new(7)).entries(&Tree::new(7))[0],
                &names
            ),
            "1"
        );
    }

    #[test]
    fn kitty_unshifted_n_with_shift_creates_a_child() {
        let tree = Tree::new(1);
        let mut explorer = Explorer::new(&tree);
        assert_eq!(
            explorer.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT),
                &tree,
                &BTreeMap::new()
            ),
            Some(Command::AddChild(1))
        );
    }

    #[test]
    fn scrolling_always_contains_selection() {
        for count in 1..100 {
            for capacity in 1..30 {
                for selected in 0..count {
                    let start = window_start(selected, count, capacity);
                    assert!(start <= selected && selected < start + capacity);
                    assert!(start <= count.saturating_sub(capacity));
                }
            }
        }
    }

    // Extract printable runs, ignoring the renderer's CSI cursor/style commands.
    fn runs(bytes: &[u8]) -> Vec<String> {
        let mut result = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == 27 {
                assert_eq!(bytes[index + 1], b'[');
                index += 2;
                while !(0x40..=0x7e).contains(&bytes[index]) {
                    index += 1;
                }
                index += 1;
            } else {
                let start = index;
                while index < bytes.len() && bytes[index] != 27 {
                    assert!(bytes[index] >= 32);
                    index += 1;
                }
                result.push(String::from_utf8(bytes[start..index].to_vec()).unwrap());
            }
        }
        result
    }

    #[test]
    fn draw_is_opaque_unicode_and_safe_at_tiny_and_maximum_sizes() {
        let tree = nested();
        let explorer = Explorer::new(&tree);
        let names = BTreeMap::from([(3, "bad\x1b[2J\n\t雪".into())]);
        for cols in 0..=80 {
            for rows in 0..=26 {
                for confirm in [false, true] {
                    let mut output = Vec::new();
                    explorer
                        .draw(&mut output, cols, rows, &tree, &names, confirm)
                        .unwrap();
                    assert!(output.starts_with(b"\x1b[?25l"));
                    let printed: usize = runs(&output).iter().map(|run| run.width()).sum();
                    let width = cols.min(if confirm { 52 } else { 78 });
                    let height = rows.min(if confirm { 5 } else { 9 });
                    assert_eq!(printed, usize::from(width) * usize::from(height));
                }
            }
        }
        let mut output = Vec::new();
        explorer
            .draw(&mut output, u16::MAX, u16::MAX, &tree, &names, false)
            .unwrap();
        assert!(output.len() < 3000);
        assert!(runs(&output).join("").contains("bad�[2J��雪"));
    }

    #[test]
    fn confirmation_replaces_explorer_and_rendering_does_not_change_selection() {
        let tree = nested();
        let explorer = Explorer::new(&tree);
        let mut output = Vec::new();
        explorer
            .draw(&mut output, 80, 24, &tree, &BTreeMap::new(), true)
            .unwrap();
        let printed = runs(&output).join("");
        assert!(printed.contains("Close running programs?"));
        assert!(printed.contains("↵ close · esc cancel"));
        assert!(!printed.contains("Sessions"));
        assert_eq!(explorer.selected(&tree), Some(3));
    }

    #[test]
    fn selected_last_item_is_rendered_in_a_long_tree() {
        let mut tree = Tree::new(1);
        tree.add(1, 2, 1000);
        for id in 3..=100 {
            tree.add(1000, id, 0);
        }
        let explorer = Explorer::new(&tree);
        let mut output = Vec::new();
        explorer
            .draw(&mut output, 80, 10, &tree, &BTreeMap::new(), false)
            .unwrap();
        let printed = runs(&output).join("");
        assert!(printed.contains('›'));
        assert!(printed.contains("100 ●"));
        assert!(!printed.contains("Session 100"));
    }

    #[test]
    fn unicode_clipping_preserves_graphemes_and_cell_width() {
        assert_eq!(fit("雪abc", 4), "雪ab");
        assert_eq!(fit("雪", 1), " ");
        assert_eq!(fit("e\u{301}x", 1), "e\u{301}");
        assert_eq!(fit("👩‍💻x", 2), "👩‍💻");
        assert_eq!(fit("\u{301}x", 2), "x ");
        assert_eq!(fit("\x1b[2J\n", 6), "�[2J� ");
        assert_eq!(tail("abc雪e\u{301}", 3), "雪e\u{301}");
        assert_eq!(tail("abc雪", 1), "");
    }

    #[test]
    fn closing_siblings_renumbers_groups_and_leaves_without_exposing_ids() {
        let mut tree = nested();
        tree.remove(1);
        let explorer = Explorer::new(&tree);
        let labels: Vec<_> = explorer
            .entries(&tree)
            .iter()
            .map(|entry| label(entry, &BTreeMap::new()))
            .collect();
        assert_eq!(labels, ["All sessions", "1", "1.1", "1.2"]);
    }

    #[test]
    fn draw_propagates_write_errors() {
        struct Broken;
        impl Write for Broken {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("broken"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        assert!(
            Explorer::new(&Tree::default())
                .draw(
                    &mut Broken,
                    80,
                    24,
                    &Tree::default(),
                    &BTreeMap::new(),
                    false
                )
                .is_err()
        );
    }
}
