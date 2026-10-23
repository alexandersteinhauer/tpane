//! Crossterm events to the core's host-independent input events.
//!
//! Crossterm reports logical characters (already shifted, sometimes C0 control
//! characters) and has no layout data, so this reconstructs the physical key,
//! the unshifted character and Shift for ASCII only.

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode, ModifierKeyCode,
    MouseButton, MouseEvent, MouseEventKind,
};
use tpane_core::{
    Key, KeyAction, Modifiers, MouseAction,
    event::{self, us_shifted, us_unshifted},
};

/// `None` for characters that no key produces (unknown C0 controls).
pub fn key(event: KeyEvent) -> Option<event::KeyEvent> {
    let mut mods = modifiers(event.modifiers);
    mods.caps_lock = event.state.contains(KeyEventState::CAPS_LOCK);
    mods.num_lock = event.state.contains(KeyEventState::NUM_LOCK);
    let mut code = event.code;
    // Null is the legacy Ctrl-Space representation.
    if code == KeyCode::Null {
        code = KeyCode::Char(' ');
        mods.ctrl = true;
    }
    if code == KeyCode::BackTab {
        mods.shift = true;
    }
    let keypad = event.state.contains(KeyEventState::KEYPAD);
    let mut translated = event::KeyEvent {
        key: key_code(code, keypad),
        text: None,
        unshifted: None,
        mods,
        action: match event.kind {
            KeyEventKind::Press => KeyAction::Press,
            KeyEventKind::Repeat => KeyAction::Repeat,
            KeyEventKind::Release => KeyAction::Release,
        },
    };
    if let KeyCode::Char(mut ch) = code {
        if ch.is_control() {
            // Host parsers normally return logical characters plus CTRL, but
            // tolerate explicitly constructed C0 character events too.
            ch = match ch {
                '\0' => ' ',
                '\x01'..='\x1a' => char::from(b'a' + ch as u8 - 1),
                '\x1b' => '[',
                '\x1c' => '\\',
                '\x1d' => ']',
                '\x1e' => '^',
                '\x1f' => '_',
                '\x7f' => '?',
                _ => return None,
            };
            mods.ctrl = true;
            translated.key = Key::from_us_char(ch);
        }
        // Kitty may supply an unshifted ASCII key with SHIFT; legacy host
        // events instead usually contain the already shifted character.
        if mods.shift && !keypad && !mods.caps_lock {
            ch = us_shifted(ch);
        }
        if ch.is_ascii_uppercase() && !mods.caps_lock {
            mods.shift = true;
        }
        // Do not unshift a symbol unless SHIFT is known. A host may have
        // produced it using another layout (or discarded Kitty alternates).
        let base = if mods.shift {
            us_unshifted(ch)
        } else {
            ch.to_ascii_lowercase()
        };
        translated.text = Some(ch.to_string());
        translated.unshifted = Some(base);
        translated.mods = mods;
    }
    Some(translated)
}

pub fn mouse(event: MouseEvent) -> event::MouseEvent {
    use tpane_core::MouseButton as B;
    let (action, button) = match event.kind {
        MouseEventKind::Down(button) => (MouseAction::Press, Some(mouse_button(button))),
        MouseEventKind::Up(button) => (MouseAction::Release, Some(mouse_button(button))),
        MouseEventKind::Drag(button) => (MouseAction::Motion, Some(mouse_button(button))),
        MouseEventKind::Moved => (MouseAction::Motion, None),
        MouseEventKind::ScrollUp => (MouseAction::Press, Some(B::WheelUp)),
        MouseEventKind::ScrollDown => (MouseAction::Press, Some(B::WheelDown)),
        MouseEventKind::ScrollLeft => (MouseAction::Press, Some(B::WheelLeft)),
        MouseEventKind::ScrollRight => (MouseAction::Press, Some(B::WheelRight)),
    };
    event::MouseEvent {
        action,
        button,
        x: event.column,
        y: event.row,
        mods: modifiers(event.modifiers),
    }
}

fn modifiers(mods: KeyModifiers) -> Modifiers {
    // The core has no Hyper or Kitty Meta modifier; SUPER is its `meta`.
    Modifiers {
        shift: mods.contains(KeyModifiers::SHIFT),
        ctrl: mods.contains(KeyModifiers::CONTROL),
        alt: mods.contains(KeyModifiers::ALT),
        meta: mods.contains(KeyModifiers::SUPER),
        caps_lock: false,
        num_lock: false,
    }
}

fn mouse_button(button: MouseButton) -> tpane_core::MouseButton {
    match button {
        MouseButton::Left => tpane_core::MouseButton::Left,
        MouseButton::Middle => tpane_core::MouseButton::Middle,
        MouseButton::Right => tpane_core::MouseButton::Right,
    }
}

fn key_code(code: KeyCode, keypad: bool) -> Key {
    use Key as K;
    if keypad {
        let key = match code {
            KeyCode::Char('0') => K::Numpad0,
            KeyCode::Char('1') => K::Numpad1,
            KeyCode::Char('2') => K::Numpad2,
            KeyCode::Char('3') => K::Numpad3,
            KeyCode::Char('4') => K::Numpad4,
            KeyCode::Char('5') => K::Numpad5,
            KeyCode::Char('6') => K::Numpad6,
            KeyCode::Char('7') => K::Numpad7,
            KeyCode::Char('8') => K::Numpad8,
            KeyCode::Char('9') => K::Numpad9,
            KeyCode::Char('+') => K::NumpadAdd,
            KeyCode::Char('-') => K::NumpadSubtract,
            KeyCode::Char('*') => K::NumpadMultiply,
            KeyCode::Char('/') => K::NumpadDivide,
            KeyCode::Char('.') => K::NumpadDecimal,
            KeyCode::Char(',') => K::NumpadComma,
            KeyCode::Char('=') => K::NumpadEqual,
            KeyCode::Enter => K::NumpadEnter,
            KeyCode::Left => K::NumpadLeft,
            KeyCode::Right => K::NumpadRight,
            KeyCode::Up => K::NumpadUp,
            KeyCode::Down => K::NumpadDown,
            KeyCode::Home => K::NumpadHome,
            KeyCode::End => K::NumpadEnd,
            KeyCode::PageUp => K::NumpadPageUp,
            KeyCode::PageDown => K::NumpadPageDown,
            KeyCode::Insert => K::NumpadInsert,
            KeyCode::Delete => K::NumpadDelete,
            KeyCode::Backspace => K::NumpadBackspace,
            _ => K::Unidentified,
        };
        if key != K::Unidentified {
            return key;
        }
    }
    match code {
        KeyCode::Char(ch) => K::from_us_char(ch),
        KeyCode::Backspace => K::Backspace,
        KeyCode::Enter => K::Enter,
        KeyCode::Left => K::ArrowLeft,
        KeyCode::Right => K::ArrowRight,
        KeyCode::Up => K::ArrowUp,
        KeyCode::Down => K::ArrowDown,
        KeyCode::Home => K::Home,
        KeyCode::End => K::End,
        KeyCode::PageUp => K::PageUp,
        KeyCode::PageDown => K::PageDown,
        KeyCode::Tab | KeyCode::BackTab => K::Tab,
        KeyCode::Delete => K::Delete,
        KeyCode::Insert => K::Insert,
        // The Ghostty key set currently ends at F25.
        KeyCode::F(n @ 1..=25) => [
            K::F1,
            K::F2,
            K::F3,
            K::F4,
            K::F5,
            K::F6,
            K::F7,
            K::F8,
            K::F9,
            K::F10,
            K::F11,
            K::F12,
            K::F13,
            K::F14,
            K::F15,
            K::F16,
            K::F17,
            K::F18,
            K::F19,
            K::F20,
            K::F21,
            K::F22,
            K::F23,
            K::F24,
            K::F25,
        ][usize::from(n - 1)],
        KeyCode::Esc => K::Escape,
        KeyCode::CapsLock => K::CapsLock,
        KeyCode::ScrollLock => K::ScrollLock,
        KeyCode::NumLock => K::NumLock,
        KeyCode::PrintScreen => K::PrintScreen,
        KeyCode::Pause => K::Pause,
        KeyCode::Menu => K::ContextMenu,
        KeyCode::KeypadBegin => K::NumpadBegin,
        KeyCode::Modifier(modifier) => match modifier {
            ModifierKeyCode::LeftShift => K::ShiftLeft,
            ModifierKeyCode::RightShift => K::ShiftRight,
            ModifierKeyCode::LeftControl => K::ControlLeft,
            ModifierKeyCode::RightControl => K::ControlRight,
            ModifierKeyCode::LeftAlt => K::AltLeft,
            ModifierKeyCode::RightAlt => K::AltRight,
            ModifierKeyCode::LeftSuper => K::MetaLeft,
            ModifierKeyCode::RightSuper => K::MetaRight,
            _ => K::Unidentified,
        },
        KeyCode::Media(media) => match media {
            MediaKeyCode::PlayPause => K::MediaPlayPause,
            MediaKeyCode::Stop => K::MediaStop,
            MediaKeyCode::TrackNext => K::MediaTrackNext,
            MediaKeyCode::TrackPrevious => K::MediaTrackPrevious,
            MediaKeyCode::LowerVolume => K::AudioVolumeDown,
            MediaKeyCode::RaiseVolume => K::AudioVolumeUp,
            MediaKeyCode::MuteVolume => K::AudioVolumeMute,
            _ => K::Unidentified,
        },
        _ => K::Unidentified,
    }
}

/// The encodings Crossterm events produced before the core had its own event
/// types; the conversion must keep every one of them.
#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use tpane_core::{input::Input, vt::Terminal};

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(80, 24).unwrap()
    }

    fn encode(input: &mut Input, terminal: &Terminal<'_, '_>, event: KeyEvent) -> Vec<u8> {
        key(event).map_or_else(Vec::new, |event| input.key(terminal, &event).unwrap())
    }

    fn encode_mouse(input: &mut Input, terminal: &Terminal<'_, '_>, event: MouseEvent) -> Vec<u8> {
        input.mouse(terminal, &mouse(event)).unwrap()
    }

    #[test]
    fn control_alt_unicode_and_release() -> Result<()> {
        let terminal = terminal();
        let mut input = Input::new()?;
        for (code, mods, expected) in [
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL,
                b"\x03".as_slice(),
            ),
            (
                KeyCode::Char('c'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
                b"\x1b\x03",
            ),
            (KeyCode::Char('x'), KeyModifiers::ALT, b"\x1bx"),
            (KeyCode::Char('é'), KeyModifiers::NONE, "é".as_bytes()),
            (KeyCode::Char('🦀'), KeyModifiers::NONE, "🦀".as_bytes()),
            (KeyCode::Char('A'), KeyModifiers::SHIFT, b"A"),
            (KeyCode::Char(' '), KeyModifiers::CONTROL, b"\0"),
            (KeyCode::Char('\x03'), KeyModifiers::NONE, b"\x03"),
            (KeyCode::Null, KeyModifiers::NONE, b"\0"),
            (KeyCode::BackTab, KeyModifiers::NONE, b"\x1b[Z"),
        ] {
            assert_eq!(
                encode(&mut input, &terminal, KeyEvent::new(code, mods)),
                expected
            );
        }
        assert!(
            encode(
                &mut input,
                &terminal,
                KeyEvent::new_with_kind(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL,
                    KeyEventKind::Release,
                )
            )
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn cursor_and_kitty_modes_are_refreshed() -> Result<()> {
        let mut terminal = terminal();
        let mut input = Input::new()?;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode(&mut input, &terminal, up), b"\x1b[A");
        terminal.vt_write(b"\x1b[?1h");
        assert_eq!(encode(&mut input, &terminal, up), b"\x1bOA");
        terminal.vt_write(b"\x1b[?1l");
        assert_eq!(encode(&mut input, &terminal, up), b"\x1b[A");
        terminal.vt_write(b"\x1b[>3u");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(encode(&mut input, &terminal, ctrl_c), b"\x1b[99;5u");
        assert_eq!(
            encode(
                &mut input,
                &terminal,
                KeyEvent {
                    kind: KeyEventKind::Release,
                    ..ctrl_c
                }
            ),
            b"\x1b[99;5:3u"
        );
        terminal.vt_write(b"\x1b[<u");
        assert_eq!(encode(&mut input, &terminal, ctrl_c), b"\x03");
        Ok(())
    }

    #[test]
    fn function_key_keypad_and_kitty_text() -> Result<()> {
        let mut terminal = terminal();
        let mut input = Input::new()?;
        assert_eq!(
            encode(
                &mut input,
                &terminal,
                KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)
            ),
            b"\x1bOP"
        );
        assert_eq!(
            encode(
                &mut input,
                &terminal,
                KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE)
            ),
            b"\x1b[24~"
        );
        terminal.vt_write(b"\x1b[?1035l\x1b=");
        let keypad_one = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('1'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            KeyEventState::KEYPAD,
        );
        assert_eq!(encode(&mut input, &terminal, keypad_one), b"\x1bOq");
        terminal.vt_write(b"\x1b[>9u");
        assert_eq!(
            encode(
                &mut input,
                &terminal,
                KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE)
            ),
            b"\x1b[233u"
        );
        assert_eq!(
            encode(
                &mut input,
                &terminal,
                KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)
            ),
            b"\x1b[33u"
        );
        assert_eq!(
            encode(
                &mut input,
                &terminal,
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT)
            ),
            b"\x1b[49;2u"
        );
        Ok(())
    }

    #[test]
    fn mouse_tracking_buttons_wheel_and_resize() -> Result<()> {
        let mut terminal = terminal();
        let mut input = Input::new()?;
        let mut event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 2,
            modifiers: KeyModifiers::NONE,
        };
        assert!(encode_mouse(&mut input, &terminal, event).is_empty());
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(encode_mouse(&mut input, &terminal, event), b"\x1b[<0;5;3M");
        event.kind = MouseEventKind::Up(MouseButton::Left);
        assert_eq!(encode_mouse(&mut input, &terminal, event), b"\x1b[<0;5;3m");
        event.kind = MouseEventKind::Moved;
        assert!(encode_mouse(&mut input, &terminal, event).is_empty());
        terminal.vt_write(b"\x1b[?1002h");
        assert!(encode_mouse(&mut input, &terminal, event).is_empty());
        event.kind = MouseEventKind::Drag(MouseButton::Left);
        assert_eq!(encode_mouse(&mut input, &terminal, event), b"\x1b[<32;5;3M");
        for (kind, expected) in [
            (MouseEventKind::ScrollUp, b"\x1b[<64;5;3M".as_slice()),
            (MouseEventKind::ScrollDown, b"\x1b[<65;5;3M"),
            (MouseEventKind::ScrollLeft, b"\x1b[<66;5;3M"),
            (MouseEventKind::ScrollRight, b"\x1b[<67;5;3M"),
        ] {
            event.kind = kind;
            assert_eq!(encode_mouse(&mut input, &terminal, event), expected);
        }
        input.set_cell_size(8, 16);
        terminal.resize(120, 40, 8, 16)?;
        event.kind = MouseEventKind::Down(MouseButton::Right);
        event.column = 119;
        event.row = 39;
        event.modifiers = KeyModifiers::CONTROL;
        assert_eq!(
            encode_mouse(&mut input, &terminal, event),
            b"\x1b[<18;120;40M"
        );
        terminal.vt_write(b"\x1b[?1002l");
        assert!(encode_mouse(&mut input, &terminal, event).is_empty());
        Ok(())
    }
}
