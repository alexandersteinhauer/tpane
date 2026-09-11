//! Option-digit navigation. Keep the cursor at the explicitly selected node,
//! not its remembered active leaf, so each digit descends exactly one level.
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};

use crate::tree::{Action, Id, Tree};

const SEQUENCE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Default)]
pub struct TabKeys {
    cursor: Option<(Id, Instant)>,
    alt_held: u8,
    // Swallow releases even after switching panes, releasing Option, or a reset.
    swallowed: u16,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Forward,
    Consume,
    Select(Action),
}

impl TabKeys {
    pub fn reset(&mut self) {
        self.cursor = None;
    }

    pub fn focus_lost(&mut self) {
        self.reset();
        self.alt_held = 0;
    }

    pub fn key(&mut self, key: KeyEvent, tree: &Tree, now: Instant) -> Route {
        let alt = match key.code {
            KeyCode::Modifier(ModifierKeyCode::LeftAlt) => 1,
            KeyCode::Modifier(ModifierKeyCode::RightAlt) => 2,
            _ => 0,
        };
        if alt != 0 {
            if key.kind == KeyEventKind::Release {
                self.alt_held &= !alt;
                if self.alt_held == 0 {
                    self.reset();
                }
            } else {
                self.alt_held |= alt;
            }
            // Standalone Option events are navigation bookkeeping, not child input.
            return Route::Consume;
        }

        let digit = match key.code {
            KeyCode::Char(ch @ '0'..='9') => Some((ch as u8 - b'0') as usize),
            _ => None,
        };
        if let Some(digit) = digit {
            let bit = 1 << digit;
            if key.kind == KeyEventKind::Release && self.swallowed & bit != 0 {
                self.swallowed &= !bit;
                return Route::Consume;
            }
            if key.kind == KeyEventKind::Repeat && self.swallowed & bit != 0 {
                return Route::Consume;
            }
            if key.kind == KeyEventKind::Press {
                self.swallowed &= !bit;
            }
            if key.modifiers == KeyModifiers::ALT {
                if key.kind != KeyEventKind::Press {
                    return Route::Consume;
                }
                self.swallowed |= bit;
                let group = self
                    .cursor
                    .filter(|(_, last)| {
                        self.alt_held != 0 || now.duration_since(*last) < SEQUENCE_TIMEOUT
                    })
                    .map(|(id, _)| id)
                    .or_else(|| tree.root.as_ref().map(|root| root.id()));
                let Some(group) = group else {
                    return Route::Consume;
                };
                // An invalid digit stays at this level; never wrap to the root
                // or leak a reserved shortcut into a shell.
                self.cursor = Some((group, now));
                let index = if digit == 0 { 9 } else { digit - 1 };
                if let Some(child) = tree.tab_at(group, index) {
                    self.cursor = Some((child, now));
                    return Route::Select(Action::Select { group, child });
                }
                return Route::Consume;
            }
        }
        if key.kind != KeyEventKind::Release {
            self.reset();
            if !key.modifiers.contains(KeyModifiers::ALT) {
                self.alt_held = 0;
            }
        }
        Route::Forward
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nested() -> Tree {
        let mut tree = Tree::new(1);
        tree.add(1, 2, 10);
        tree.add(2, 3, 20);
        tree.add(3, 4, 30);
        tree
    }

    fn digit(ch: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(ch), KeyModifiers::ALT)
    }

    fn alt(kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(
            KeyCode::Modifier(ModifierKeyCode::LeftAlt),
            KeyModifiers::ALT,
            kind,
        )
    }

    fn select(keys: &mut TabKeys, tree: &mut Tree, ch: char, now: Instant, group: Id, child: Id) {
        let route = keys.key(digit(ch), tree, now);
        assert_eq!(route, Route::Select(Action::Select { group, child }));
        assert!(tree.select(group, child));
    }

    #[test]
    fn held_option_descends_one_level_and_release_restarts_at_root() {
        let mut tree = nested();
        let mut keys = TabKeys::default();
        let now = Instant::now();
        assert_eq!(
            keys.key(alt(KeyEventKind::Press), &tree, now),
            Route::Consume
        );
        select(&mut keys, &mut tree, '2', now, 10, 20);
        // Holding Option has no time limit when the host reports modifiers.
        let later = now + Duration::from_secs(10);
        select(&mut keys, &mut tree, '2', later, 20, 30);
        select(&mut keys, &mut tree, '1', later, 30, 3);
        assert_eq!(tree.active_terminal(), Some(3));
        assert_eq!(keys.key(digit('1'), &tree, later), Route::Consume); // Leaf.
        keys.key(alt(KeyEventKind::Release), &tree, later);
        select(&mut keys, &mut tree, '1', later, 10, 1);
        keys.key(alt(KeyEventKind::Release), &tree, later);
        select(&mut keys, &mut tree, '2', later, 10, 20);
        assert_eq!(tree.active_terminal(), Some(3)); // Remembered selection.
    }

    #[test]
    fn legacy_sequences_expire_and_invalid_digits_do_not_advance() {
        let mut tree = nested();
        let mut keys = TabKeys::default();
        let now = Instant::now();
        assert_eq!(keys.key(digit('9'), &tree, now), Route::Consume);
        select(&mut keys, &mut tree, '2', now, 10, 20);
        assert_eq!(keys.key(digit('9'), &tree, now), Route::Consume);
        select(&mut keys, &mut tree, '1', now, 20, 2);
        select(&mut keys, &mut tree, '1', now + SEQUENCE_TIMEOUT, 10, 1);
    }

    #[test]
    fn repeats_and_releases_never_reach_children_or_descend() {
        let mut tree = nested();
        let mut keys = TabKeys::default();
        let now = Instant::now();
        select(&mut keys, &mut tree, '2', now, 10, 20);
        let mut key = digit('2');
        key.kind = KeyEventKind::Repeat;
        assert_eq!(keys.key(key, &tree, now), Route::Consume);
        keys.key(alt(KeyEventKind::Release), &tree, now);
        keys.reset();
        key.kind = KeyEventKind::Release;
        key.modifiers = KeyModifiers::NONE;
        assert_eq!(keys.key(key, &tree, now), Route::Consume);
        select(&mut keys, &mut tree, '2', now, 10, 20);
    }

    #[test]
    fn ordinary_input_and_context_changes_end_sequences() {
        let mut tree = nested();
        let now = Instant::now();
        for key in [
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
            KeyEvent::new(
                KeyCode::Char('2'),
                KeyModifiers::ALT | KeyModifiers::CONTROL,
            ),
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        ] {
            let mut keys = TabKeys::default();
            select(&mut keys, &mut tree, '2', now, 10, 20);
            assert_eq!(keys.key(key, &tree, now), Route::Forward);
            select(&mut keys, &mut tree, '1', now, 10, 1);
        }
        let mut keys = TabKeys::default();
        select(&mut keys, &mut tree, '2', now, 10, 20);
        keys.reset();
        select(&mut keys, &mut tree, '2', now, 10, 20);
        keys.focus_lost();
        select(&mut keys, &mut tree, '1', now, 10, 1);
    }

    #[test]
    fn numbering_is_positional_zero_selects_ten_and_empty_trees_are_safe() {
        let mut tree = Tree::new(100);
        tree.add(100, 101, 200);
        for id in 102..110 {
            tree.add(200, id, 999);
        }
        let mut keys = TabKeys::default();
        let now = Instant::now();
        select(&mut keys, &mut tree, '0', now, 200, 109);
        assert_eq!(tree.tab_at(200, 9), Some(109));
        tree.remove(100);
        keys.reset();
        select(&mut keys, &mut tree, '1', now, 200, 101);
        assert_eq!(tree.tab_at(200, 0), Some(101));
        assert_eq!(keys.key(digit('1'), &Tree::default(), now), Route::Consume);
        keys.reset();
        assert_eq!(keys.key(digit('1'), &Tree::new(1), now), Route::Consume);
    }
}
