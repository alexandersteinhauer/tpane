//! Synchronous translation from host events to the child terminal's input modes.
//!
//! The caller routes events (including selection/scrollback handling) before
//! calling this module. Coordinates must be relative to the child viewport.

use anyhow::Result;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode, ModifierKeyCode,
    MouseButton, MouseEvent, MouseEventKind,
};
use libghostty_vt::{Terminal, key, mouse, terminal::Mode};

pub struct Input {
    keyboard: key::Encoder<'static>,
    mouse: mouse::Encoder<'static>,
    cell_width: u16,
    cell_height: u16,
}

impl Input {
    pub fn new() -> Result<Self> {
        let mut mouse = mouse::Encoder::new()?;
        // Host events already have cell resolution. Avoid retaining a last cell
        // across switches between child terminals with different tracking modes.
        mouse.set_track_last_cell(false);
        Ok(Self {
            keyboard: key::Encoder::new()?,
            mouse,
            cell_width: 1,
            cell_height: 1,
        })
    }

    /// Optional host cell dimensions for SGR-pixel reporting. Crossterm supplies
    /// only cell coordinates, so pixel positions remain cell-granular. Without
    /// host pixel geometry, one cell is treated as one pixel.
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        self.cell_width = width.max(1);
        self.cell_height = height.max(1);
    }

    pub fn key(&mut self, terminal: &Terminal<'_, '_>, event: KeyEvent) -> Result<Vec<u8>> {
        self.keyboard
            .set_options_from_terminal(terminal)
            // Crossterm's ALT is already a logical Alt, not an Option key that
            // should be used to compose text. Terminal options reset this flag.
            .set_macos_option_as_alt(key::OptionAsAlt::True);

        let mut mods = modifiers(event.modifiers);
        mods.set(
            key::Mods::CAPS_LOCK,
            event.state.contains(KeyEventState::CAPS_LOCK),
        );
        mods.set(
            key::Mods::NUM_LOCK,
            event.state.contains(KeyEventState::NUM_LOCK),
        );
        let mut code = event.code;
        // Null is the legacy Ctrl-Space representation. Never hand Ghostty C0
        // text: its encoder must perform the control transformation itself.
        if code == KeyCode::Null {
            code = KeyCode::Char(' ');
            mods.insert(key::Mods::CTRL);
        }
        if code == KeyCode::BackTab {
            mods.insert(key::Mods::SHIFT);
        }

        let mut translated = key::Event::new()?;
        translated.set_action(match event.kind {
            KeyEventKind::Press => key::Action::Press,
            KeyEventKind::Repeat => key::Action::Repeat,
            KeyEventKind::Release => key::Action::Release,
        });
        let physical = key_code(code, event.state.contains(KeyEventState::KEYPAD));
        translated.set_key(physical);
        if let KeyCode::Char(mut ch) = code {
            if ch.is_control() {
                // Host parsers normally return logical characters plus CTRL,
                // but tolerate explicitly constructed C0 character events too.
                ch = match ch {
                    '\0' => ' ',
                    '\x01'..='\x1a' => char::from(b'a' + ch as u8 - 1),
                    '\x1b' => '[',
                    '\x1c' => '\\',
                    '\x1d' => ']',
                    '\x1e' => '^',
                    '\x1f' => '_',
                    '\x7f' => '?',
                    _ => return Ok(Vec::new()),
                };
                mods.insert(key::Mods::CTRL);
                translated.set_key(character_key(ch));
            }
            // Kitty may supply an unshifted ASCII key with SHIFT; legacy host
            // events instead usually contain the already shifted character.
            // Crossterm has no layout data, so only reconstruct ASCII here.
            if mods.contains(key::Mods::SHIFT)
                && !event.state.contains(KeyEventState::KEYPAD)
                && !mods.contains(key::Mods::CAPS_LOCK)
            {
                ch = shifted(ch);
            }
            if ch.is_ascii_uppercase() && !mods.contains(key::Mods::CAPS_LOCK) {
                mods.insert(key::Mods::SHIFT);
            }
            // Do not unshift a symbol unless SHIFT is known. A host may have
            // produced it using another layout (or discarded Kitty alternates).
            let base = if mods.contains(key::Mods::SHIFT) {
                unshifted(ch)
            } else {
                ch.to_ascii_lowercase()
            };
            translated
                .set_utf8(Some(ch.to_string()))
                .set_unshifted_codepoint(base)
                .set_consumed_mods(mods & key::Mods::SHIFT);
        }
        translated.set_mods(mods);
        let mut bytes = Vec::with_capacity(128);
        // Do not discard releases here: Ghostty suppresses them in legacy mode
        // but emits them when the child requests Kitty event reporting.
        self.keyboard.encode_to_vec(&translated, &mut bytes)?;
        Ok(bytes)
    }

    pub fn mouse(&mut self, terminal: &Terminal<'_, '_>, event: MouseEvent) -> Result<Vec<u8>> {
        let width = u32::from(self.cell_width);
        let height = u32::from(self.cell_height);
        self.mouse
            .set_options_from_terminal(terminal)
            .set_size(mouse::EncoderSize {
                screen_width: u32::from(terminal.cols()?) * width,
                screen_height: u32::from(terminal.rows()?) * height,
                cell_width: width,
                cell_height: height,
                padding_top: 0,
                padding_bottom: 0,
                padding_left: 0,
                padding_right: 0,
            })
            .set_any_button_pressed(matches!(
                event.kind,
                MouseEventKind::Down(_) | MouseEventKind::Drag(_)
            ));
        let (action, button) = match event.kind {
            MouseEventKind::Down(button) => (mouse::Action::Press, Some(mouse_button(button))),
            MouseEventKind::Up(button) => (mouse::Action::Release, Some(mouse_button(button))),
            MouseEventKind::Drag(button) => (mouse::Action::Motion, Some(mouse_button(button))),
            MouseEventKind::Moved => (mouse::Action::Motion, None),
            MouseEventKind::ScrollUp => (mouse::Action::Press, Some(mouse::Button::Four)),
            MouseEventKind::ScrollDown => (mouse::Action::Press, Some(mouse::Button::Five)),
            MouseEventKind::ScrollLeft => (mouse::Action::Press, Some(mouse::Button::Six)),
            MouseEventKind::ScrollRight => (mouse::Action::Press, Some(mouse::Button::Seven)),
        };
        let mut translated = mouse::Event::new()?;
        translated
            .set_action(action)
            .set_button(button)
            .set_mods(modifiers(event.modifiers))
            .set_position(mouse::Position {
                x: f32::from(event.column) * f32::from(self.cell_width),
                y: f32::from(event.row) * f32::from(self.cell_height),
            });
        let mut bytes = Vec::with_capacity(64);
        self.mouse.encode_to_vec(&translated, &mut bytes)?;
        Ok(bytes)
    }
}

pub fn paste(terminal: &Terminal<'_, '_>, text: &str) -> Result<Vec<u8>> {
    let mut data = text.as_bytes().to_vec();
    // Encoding only replaces bytes, plus at most two six-byte delimiters.
    let mut bytes = vec![0; data.len() + 12];
    let written =
        libghostty_vt::paste::encode(&mut data, terminal.mode(Mode::BRACKETED_PASTE)?, &mut bytes)?;
    bytes.truncate(written);
    Ok(bytes)
}

pub fn focus(terminal: &Terminal<'_, '_>, gained: bool) -> Result<Vec<u8>> {
    if !terminal.mode(Mode::FOCUS_EVENT)? {
        return Ok(Vec::new());
    }
    let event = if gained {
        libghostty_vt::focus::Event::Gained
    } else {
        libghostty_vt::focus::Event::Lost
    };
    let mut bytes = vec![0; 3];
    let written = event.encode(&mut bytes)?;
    bytes.truncate(written);
    Ok(bytes)
}

fn modifiers(mods: KeyModifiers) -> key::Mods {
    let mut result = key::Mods::empty();
    for (host, child) in [
        (KeyModifiers::SHIFT, key::Mods::SHIFT),
        (KeyModifiers::CONTROL, key::Mods::CTRL),
        (KeyModifiers::ALT, key::Mods::ALT),
        (KeyModifiers::SUPER, key::Mods::SUPER),
    ] {
        result.set(child, mods.contains(host));
    }
    // libghostty-vt has no Hyper or Kitty Meta modifier bits.
    result
}

fn mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Right => mouse::Button::Right,
    }
}

fn unshifted(ch: char) -> char {
    // Crossterm does not retain physical/layout or Kitty alternate key codes.
    // Reconstruct ASCII only; preserve non-ASCII codepoints without guessing a
    // keyboard layout or performing potentially multi-codepoint case folding.
    match ch {
        '~' => '`',
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        '_' => '-',
        '+' => '=',
        '{' => '[',
        '}' => ']',
        '|' => '\\',
        ':' => ';',
        '"' => '\'',
        '<' => ',',
        '>' => '.',
        '?' => '/',
        _ => ch.to_ascii_lowercase(),
    }
}

fn shifted(ch: char) -> char {
    const BASE: &str = "`1234567890-=[]\\;',./";
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    BASE.chars()
        .zip(SHIFTED.chars())
        .find_map(|(base, shifted)| (ch == base).then_some(shifted))
        .unwrap_or_else(|| ch.to_ascii_uppercase())
}

fn character_key(ch: char) -> key::Key {
    use key::Key as K;
    match unshifted(ch) {
        'a' => K::A,
        'b' => K::B,
        'c' => K::C,
        'd' => K::D,
        'e' => K::E,
        'f' => K::F,
        'g' => K::G,
        'h' => K::H,
        'i' => K::I,
        'j' => K::J,
        'k' => K::K,
        'l' => K::L,
        'm' => K::M,
        'n' => K::N,
        'o' => K::O,
        'p' => K::P,
        'q' => K::Q,
        'r' => K::R,
        's' => K::S,
        't' => K::T,
        'u' => K::U,
        'v' => K::V,
        'w' => K::W,
        'x' => K::X,
        'y' => K::Y,
        'z' => K::Z,
        '0' => K::Digit0,
        '1' => K::Digit1,
        '2' => K::Digit2,
        '3' => K::Digit3,
        '4' => K::Digit4,
        '5' => K::Digit5,
        '6' => K::Digit6,
        '7' => K::Digit7,
        '8' => K::Digit8,
        '9' => K::Digit9,
        '`' => K::Backquote,
        '\\' => K::Backslash,
        '[' => K::BracketLeft,
        ']' => K::BracketRight,
        ',' => K::Comma,
        '=' => K::Equal,
        '-' => K::Minus,
        '.' => K::Period,
        '\'' => K::Quote,
        ';' => K::Semicolon,
        '/' => K::Slash,
        ' ' => K::Space,
        _ => K::Unidentified,
    }
}

fn key_code(code: KeyCode, keypad: bool) -> key::Key {
    use key::Key as K;
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
        KeyCode::Char(ch) => character_key(ch),
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
        // The Ghostty public key enum currently ends at F25.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(80, 24).unwrap()
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
            (KeyCode::BackTab, KeyModifiers::NONE, b"\x1b[Z"),
        ] {
            assert_eq!(input.key(&terminal, KeyEvent::new(code, mods))?, expected);
        }
        assert!(
            input
                .key(
                    &terminal,
                    KeyEvent::new_with_kind(
                        KeyCode::Char('c'),
                        KeyModifiers::CONTROL,
                        KeyEventKind::Release,
                    )
                )?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn cursor_and_kitty_modes_are_refreshed() -> Result<()> {
        let mut terminal = terminal();
        let mut input = Input::new()?;
        let up = KeyEvent::new(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(input.key(&terminal, up)?, b"\x1b[A");
        terminal.vt_write(b"\x1b[?1h");
        assert_eq!(input.key(&terminal, up)?, b"\x1bOA");
        terminal.vt_write(b"\x1b[?1l");
        assert_eq!(input.key(&terminal, up)?, b"\x1b[A");
        terminal.vt_write(b"\x1b[>3u");
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(input.key(&terminal, ctrl_c)?, b"\x1b[99;5u");
        assert_eq!(
            input.key(
                &terminal,
                KeyEvent {
                    kind: KeyEventKind::Release,
                    ..ctrl_c
                }
            )?,
            b"\x1b[99;5:3u"
        );
        terminal.vt_write(b"\x1b[<u");
        assert_eq!(input.key(&terminal, ctrl_c)?, b"\x03");
        Ok(())
    }

    #[test]
    fn function_key_keypad_and_kitty_text() -> Result<()> {
        let mut terminal = terminal();
        let mut input = Input::new()?;
        assert_eq!(
            input.key(&terminal, KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))?,
            b"\x1bOP"
        );
        assert_eq!(
            input.key(&terminal, KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE))?,
            b"\x1b[24~"
        );
        terminal.vt_write(b"\x1b[?1035l\x1b=");
        let keypad_one = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('1'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            KeyEventState::KEYPAD,
        );
        assert_eq!(input.key(&terminal, keypad_one)?, b"\x1bOq");
        terminal.vt_write(b"\x1b[>9u");
        assert_eq!(
            input.key(
                &terminal,
                KeyEvent::new(KeyCode::Char('é'), KeyModifiers::NONE)
            )?,
            b"\x1b[233u"
        );
        assert_eq!(
            input.key(
                &terminal,
                KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE)
            )?,
            b"\x1b[33u"
        );
        assert_eq!(
            input.key(
                &terminal,
                KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT)
            )?,
            b"\x1b[49;2u"
        );
        Ok(())
    }

    #[test]
    fn paste_and_focus_follow_terminal_modes() -> Result<()> {
        let mut terminal = terminal();
        assert_eq!(paste(&terminal, "é\nhello")?, "é\rhello".as_bytes());
        terminal.vt_write(b"\x1b[?2004h");
        assert_eq!(
            paste(&terminal, "é\nhello")?,
            "\x1b[200~é\nhello\x1b[201~".as_bytes()
        );
        assert_eq!(
            paste(&terminal, "\x1b[201~\0")?,
            b"\x1b[200~ [201~ \x1b[201~"
        );
        terminal.vt_write(b"\x1b[?2004l");
        assert_eq!(paste(&terminal, "hello")?, b"hello");
        assert!(focus(&terminal, true)?.is_empty());
        terminal.vt_write(b"\x1b[?1004h");
        assert_eq!(focus(&terminal, true)?, b"\x1b[I");
        assert_eq!(focus(&terminal, false)?, b"\x1b[O");
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
        assert!(input.mouse(&terminal, event)?.is_empty());
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(input.mouse(&terminal, event)?, b"\x1b[<0;5;3M");
        event.kind = MouseEventKind::Up(MouseButton::Left);
        assert_eq!(input.mouse(&terminal, event)?, b"\x1b[<0;5;3m");
        event.kind = MouseEventKind::Moved;
        assert!(input.mouse(&terminal, event)?.is_empty());
        terminal.vt_write(b"\x1b[?1002h");
        assert!(input.mouse(&terminal, event)?.is_empty());
        event.kind = MouseEventKind::Drag(MouseButton::Left);
        assert_eq!(input.mouse(&terminal, event)?, b"\x1b[<32;5;3M");
        for (kind, expected) in [
            (MouseEventKind::ScrollUp, b"\x1b[<64;5;3M".as_slice()),
            (MouseEventKind::ScrollDown, b"\x1b[<65;5;3M"),
            (MouseEventKind::ScrollLeft, b"\x1b[<66;5;3M"),
            (MouseEventKind::ScrollRight, b"\x1b[<67;5;3M"),
        ] {
            event.kind = kind;
            assert_eq!(input.mouse(&terminal, event)?, expected);
        }
        input.set_cell_size(8, 16);
        terminal.resize(120, 40, 8, 16)?;
        event.kind = MouseEventKind::Down(MouseButton::Right);
        event.column = 119;
        event.row = 39;
        event.modifiers = KeyModifiers::CONTROL;
        assert_eq!(input.mouse(&terminal, event)?, b"\x1b[<18;120;40M");
        terminal.vt_write(b"\x1b[?1002l");
        assert!(input.mouse(&terminal, event)?.is_empty());
        Ok(())
    }
}
