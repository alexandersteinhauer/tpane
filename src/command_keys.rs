//! Explorer-toggle gestures, independent of pane/navigation state.
//!
//! Forward Option events to `TabKeys`. When an Option release returns `Toggle`,
//! the caller must call `tab_keys.focus_lost()` (not just its sequence reset),
//! since that release is not forwarded. Call `reset()` on focus loss, paste,
//! mouse input, and resize.
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, ModifierKeyCode};

const TAP_WINDOW: Duration = Duration::from_millis(400);

#[derive(Default)]
pub struct CommandKeys {
    alt_held: u8,
    pressed_at: Option<Instant>,
    released_at: Option<Instant>,
    other_held: HashSet<KeyCode>,
    // Keep swallowing a shortcut's tail across context changes/modifier release.
    shortcut_held: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    Forward,
    Consume,
    Toggle,
}

impl CommandKeys {
    pub fn reset(&mut self) {
        self.alt_held = 0;
        self.other_held.clear();
        self.cancel_taps();
    }

    fn cancel_taps(&mut self) {
        self.pressed_at = None;
        self.released_at = None;
    }

    pub fn key(&mut self, key: KeyEvent, now: Instant) -> Route {
        let alt = match key.code {
            KeyCode::Modifier(ModifierKeyCode::LeftAlt) => 1,
            KeyCode::Modifier(ModifierKeyCode::RightAlt) => 2,
            _ => 0,
        };
        if alt != 0 {
            // Release reports may omit ALT; any additional modifier disqualifies
            // the entire cycle. Track both sides even after a cycle is spoiled.
            if !(key.modifiers - KeyModifiers::ALT).is_empty() {
                self.cancel_taps();
            }
            match key.kind {
                KeyEventKind::Press => {
                    let clean = self.alt_held == 0
                        && self.other_held.is_empty()
                        && (key.modifiers - KeyModifiers::ALT).is_empty();
                    self.alt_held |= alt;
                    if clean {
                        self.pressed_at = Some(now);
                    } else {
                        self.cancel_taps();
                    }
                }
                KeyEventKind::Repeat => {
                    self.alt_held |= alt;
                    self.cancel_taps();
                }
                KeyEventKind::Release => {
                    let clean = self.alt_held == alt
                        && self
                            .pressed_at
                            .is_some_and(|start| within_window(start, now));
                    self.alt_held &= !alt;
                    self.pressed_at = None;
                    if !clean {
                        self.cancel_taps();
                    } else if self
                        .released_at
                        .is_some_and(|last| within_window(last, now))
                    {
                        self.released_at = None;
                        return Route::Toggle;
                    } else {
                        // An expired pair can start a fresh pair with this tap.
                        self.released_at = Some(now);
                    }
                }
            }
            return Route::Forward;
        }

        // Every other key event, including releases and Option-digit shortcuts,
        // invalidates the candidate. Never consume Option-digit navigation.
        self.cancel_taps();
        let shortcut_code = matches!(key.code, KeyCode::Char(' ' | '@') | KeyCode::Null);
        // Hosts may change the code from Null/@ to Space when Control is lifted.
        let held_code = if shortcut_code {
            KeyCode::Char(' ')
        } else {
            key.code
        };
        if key.kind == KeyEventKind::Release {
            self.other_held.remove(&held_code);
        } else {
            self.other_held.insert(held_code);
        }
        if !shortcut_code {
            return Route::Forward;
        }

        if self.shortcut_held && key.kind != KeyEventKind::Press {
            if key.kind == KeyEventKind::Release {
                self.shortcut_held = false;
            }
            return Route::Consume;
        }
        if key.kind == KeyEventKind::Press {
            // Legacy hosts send only Press, so a fresh Press must rearm even
            // without a previous release. Explicit Repeat never toggles.
            self.shortcut_held = false;
        }
        let shortcut = key.modifiers == KeyModifiers::CONTROL
            || (key.code == KeyCode::Null && key.modifiers.is_empty());
        if shortcut {
            if key.kind == KeyEventKind::Press {
                self.shortcut_held = true;
                return Route::Toggle;
            }
            return Route::Consume;
        }
        Route::Forward
    }
}

fn within_window(start: Instant, now: Instant) -> bool {
    now.checked_duration_since(start)
        .is_some_and(|elapsed| elapsed <= TAP_WINDOW)
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyEventKind::{Press, Release, Repeat};

    fn event(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(code, modifiers, kind)
    }

    fn alt(side: ModifierKeyCode, kind: KeyEventKind) -> KeyEvent {
        event(KeyCode::Modifier(side), KeyModifiers::ALT, kind)
    }

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    fn tap(keys: &mut CommandKeys, start: Instant, ms: u64, side: ModifierKeyCode) -> Route {
        assert_eq!(keys.key(alt(side, Press), at(start, ms)), Route::Forward);
        keys.key(alt(side, Release), at(start, ms + 20))
    }

    #[test]
    fn two_clean_cycles_toggle_only_on_second_release() {
        for side in [ModifierKeyCode::LeftAlt, ModifierKeyCode::RightAlt] {
            let mut keys = CommandKeys::default();
            let now = Instant::now();
            assert_eq!(tap(&mut keys, now, 0, side), Route::Forward);
            assert_eq!(tap(&mut keys, now, 100, side), Route::Toggle);
            assert_eq!(tap(&mut keys, now, 200, side), Route::Forward);
            assert_eq!(tap(&mut keys, now, 300, side), Route::Toggle);
        }
    }

    #[test]
    fn nonoverlapping_sides_and_modifierless_releases_are_allowed() {
        let mut keys = CommandKeys::default();
        let now = Instant::now();
        assert_eq!(
            tap(&mut keys, now, 0, ModifierKeyCode::LeftAlt),
            Route::Forward
        );
        assert_eq!(
            keys.key(alt(ModifierKeyCode::RightAlt, Press), at(now, 100)),
            Route::Forward
        );
        assert_eq!(
            keys.key(
                event(
                    KeyCode::Modifier(ModifierKeyCode::RightAlt),
                    KeyModifiers::NONE,
                    Release
                ),
                at(now, 120)
            ),
            Route::Toggle
        );
    }

    #[test]
    fn timeout_boundary_and_expired_pair_recovery() {
        for (delay, expected) in [(400, Route::Toggle), (401, Route::Forward)] {
            let mut keys = CommandKeys::default();
            let now = Instant::now();
            assert_eq!(
                tap(&mut keys, now, 0, ModifierKeyCode::LeftAlt),
                Route::Forward
            );
            assert_eq!(
                tap(&mut keys, now, delay, ModifierKeyCode::LeftAlt),
                expected
            );
            if delay == 401 {
                assert_eq!(
                    tap(&mut keys, now, 450, ModifierKeyCode::LeftAlt),
                    Route::Toggle
                );
            }
        }
    }

    #[test]
    fn long_holds_repeats_and_duplicate_presses_spoil_either_tap() {
        let side = ModifierKeyCode::LeftAlt;
        for first_tap in [false, true] {
            for spoil in [None, Some(Repeat), Some(Press)] {
                let mut keys = CommandKeys::default();
                let now = Instant::now();
                if first_tap {
                    assert_eq!(tap(&mut keys, now, 0, side), Route::Forward);
                }
                assert_eq!(keys.key(alt(side, Press), at(now, 50)), Route::Forward);
                if let Some(kind) = spoil {
                    assert_eq!(keys.key(alt(side, kind), at(now, 60)), Route::Forward);
                }
                let release = if spoil.is_none() { 451 } else { 70 };
                assert_eq!(
                    keys.key(alt(side, Release), at(now, release)),
                    Route::Forward
                );
                assert_eq!(tap(&mut keys, now, release + 10, side), Route::Forward);
                assert_eq!(tap(&mut keys, now, release + 50, side), Route::Toggle);
            }
        }
    }

    #[test]
    fn other_keys_between_or_during_taps_cancel_without_swallowing() {
        let side = ModifierKeyCode::LeftAlt;
        for during in [false, true] {
            for kind in [Press, Repeat, Release] {
                for code in [
                    KeyCode::Char('1'),
                    KeyCode::Char('0'),
                    KeyCode::Char('x'),
                    KeyCode::Esc,
                    KeyCode::Modifier(ModifierKeyCode::LeftShift),
                ] {
                    for modifiers in [KeyModifiers::NONE, KeyModifiers::ALT] {
                        let mut keys = CommandKeys::default();
                        let now = Instant::now();
                        assert_eq!(tap(&mut keys, now, 0, side), Route::Forward);
                        if during {
                            keys.key(alt(side, Press), at(now, 30));
                        }
                        assert_eq!(
                            keys.key(event(code, modifiers, kind), at(now, 40)),
                            Route::Forward
                        );
                        keys.key(event(code, KeyModifiers::NONE, Release), at(now, 50));
                        if during {
                            assert_eq!(keys.key(alt(side, Release), at(now, 60)), Route::Forward);
                        }
                        assert_eq!(tap(&mut keys, now, 100, side), Route::Forward);
                        assert_eq!(tap(&mut keys, now, 150, side), Route::Toggle);
                    }
                }
            }
        }
    }

    #[test]
    fn overlapping_sides_and_orphan_releases_do_not_count() {
        let now = Instant::now();
        let left = ModifierKeyCode::LeftAlt;
        let right = ModifierKeyCode::RightAlt;
        for releases in [[left, right], [right, left]] {
            let mut keys = CommandKeys::default();
            tap(&mut keys, now, 0, left);
            for (ms, side, kind) in [
                (30, left, Press),
                (40, right, Press),
                (50, releases[0], Release),
                (60, releases[1], Release),
                (70, left, Release),
            ] {
                assert_eq!(keys.key(alt(side, kind), at(now, ms)), Route::Forward);
            }
            assert_eq!(tap(&mut keys, now, 100, left), Route::Forward);
            assert_eq!(tap(&mut keys, now, 150, left), Route::Toggle);
        }
    }

    #[test]
    fn preheld_keys_and_extra_modifiers_disqualify_taps() {
        let now = Instant::now();
        let side = ModifierKeyCode::LeftAlt;
        let mut keys = CommandKeys::default();
        keys.key(event(KeyCode::Char('x'), KeyModifiers::NONE, Press), now);
        assert_eq!(tap(&mut keys, now, 10, side), Route::Forward);
        assert_eq!(tap(&mut keys, now, 50, side), Route::Forward);
        keys.key(
            event(KeyCode::Char('x'), KeyModifiers::NONE, Release),
            at(now, 80),
        );
        assert_eq!(tap(&mut keys, now, 100, side), Route::Forward);
        for extra in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            for kind in [Press, Release] {
                let mut keys = CommandKeys::default();
                tap(&mut keys, now, 0, side);
                let mut press = alt(side, Press);
                let mut release = alt(side, Release);
                if kind == Press {
                    press.modifiers |= extra;
                } else {
                    release.modifiers |= extra;
                }
                assert_eq!(keys.key(press, at(now, 50)), Route::Forward);
                assert_eq!(keys.key(release, at(now, 70)), Route::Forward);
                assert_eq!(tap(&mut keys, now, 100, side), Route::Forward);
            }
        }
    }

    #[test]
    fn reset_cancels_pending_and_in_progress_taps() {
        for held in [false, true] {
            let mut keys = CommandKeys::default();
            let now = Instant::now();
            let side = ModifierKeyCode::LeftAlt;
            tap(&mut keys, now, 0, side);
            if held {
                keys.key(alt(side, Press), at(now, 30));
            }
            keys.reset();
            if held {
                assert_eq!(keys.key(alt(side, Release), at(now, 40)), Route::Forward);
            }
            assert_eq!(tap(&mut keys, now, 50, side), Route::Forward);
            assert_eq!(tap(&mut keys, now, 100, side), Route::Toggle);
        }
    }

    #[test]
    fn fallback_aliases_swallow_tails_across_modifier_code_and_context_changes() {
        for code in [KeyCode::Char(' '), KeyCode::Char('@'), KeyCode::Null] {
            for release_code in [KeyCode::Char(' '), KeyCode::Char('@'), KeyCode::Null] {
                let mut keys = CommandKeys::default();
                let now = Instant::now();
                assert_eq!(
                    keys.key(event(code, KeyModifiers::CONTROL, Press), now),
                    Route::Toggle
                );
                keys.reset();
                assert_eq!(
                    keys.key(event(release_code, KeyModifiers::SHIFT, Repeat), now),
                    Route::Consume
                );
                assert_eq!(
                    keys.key(event(release_code, KeyModifiers::NONE, Release), now),
                    Route::Consume
                );
                assert_eq!(
                    keys.key(event(KeyCode::Char(' '), KeyModifiers::NONE, Press), now),
                    Route::Forward
                );
                assert_eq!(
                    keys.key(event(KeyCode::Char(' '), KeyModifiers::NONE, Release), now),
                    Route::Forward
                );
            }
        }
    }

    #[test]
    fn legacy_null_and_press_only_fallback_work_but_other_chords_do_not() {
        let mut keys = CommandKeys::default();
        let now = Instant::now();
        for _ in 0..2 {
            assert_eq!(
                keys.key(event(KeyCode::Null, KeyModifiers::NONE, Press), now),
                Route::Toggle
            );
        }
        for code in [KeyCode::Char(' '), KeyCode::Char('@'), KeyCode::Null] {
            assert_eq!(
                keys.key(event(code, KeyModifiers::CONTROL, Repeat), now),
                Route::Consume
            );
            assert_eq!(
                keys.key(
                    event(code, KeyModifiers::CONTROL | KeyModifiers::ALT, Press),
                    now
                ),
                Route::Forward
            );
            assert_eq!(
                keys.key(
                    event(code, KeyModifiers::CONTROL | KeyModifiers::SHIFT, Press),
                    now
                ),
                Route::Forward
            );
        }
        assert_eq!(
            keys.key(event(KeyCode::Char('@'), KeyModifiers::NONE, Press), now),
            Route::Forward
        );
    }

    #[test]
    fn fallback_cancels_option_candidate() {
        let mut keys = CommandKeys::default();
        let now = Instant::now();
        let side = ModifierKeyCode::LeftAlt;
        tap(&mut keys, now, 0, side);
        assert_eq!(
            keys.key(
                event(KeyCode::Char(' '), KeyModifiers::CONTROL, Press),
                at(now, 30)
            ),
            Route::Toggle
        );
        assert_eq!(
            keys.key(
                event(KeyCode::Char(' '), KeyModifiers::NONE, Release),
                at(now, 40)
            ),
            Route::Consume
        );
        assert_eq!(tap(&mut keys, now, 50, side), Route::Forward);
    }
}
