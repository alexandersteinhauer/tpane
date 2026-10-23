//! Synchronous encoding of host-independent input events for the child,
//! according to the terminal's current modes.
//!
//! The caller routes events (including selection/scrollback handling) before
//! calling this module. Coordinates must be relative to the child viewport.

use anyhow::Result;
use libghostty_vt::{Terminal, key, mouse, terminal::Mode};

use crate::event::{KeyAction, KeyEvent, Modifiers, MouseAction, MouseButton, MouseEvent};

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

    /// Optional host cell dimensions for SGR-pixel reporting. Events carry
    /// only cell coordinates, so pixel positions remain cell-granular. Without
    /// host pixel geometry, one cell is treated as one pixel.
    pub fn set_cell_size(&mut self, width: u16, height: u16) {
        self.cell_width = width.max(1);
        self.cell_height = height.max(1);
    }

    pub fn key(&mut self, terminal: &Terminal<'_, '_>, event: &KeyEvent) -> Result<Vec<u8>> {
        self.keyboard
            .set_options_from_terminal(terminal)
            // Hosts report a logical Alt, not an Option key that should be
            // used to compose text. Terminal options reset this flag.
            .set_macos_option_as_alt(key::OptionAsAlt::True);
        let mods = modifiers(event.mods);
        let mut translated = key::Event::new()?;
        translated.set_action(match event.action {
            KeyAction::Press => key::Action::Press,
            KeyAction::Repeat => key::Action::Repeat,
            KeyAction::Release => key::Action::Release,
        });
        translated.set_key(event.key.to_ghostty());
        // Never hand Ghostty C0 text: its encoder must perform the control
        // transformation itself.
        if let Some(text) = &event.text
            && !text.is_empty()
            && !text.chars().any(char::is_control)
        {
            let base = event
                .unshifted
                .or_else(|| event.key.us_char())
                .unwrap_or_else(|| {
                    let mut chars = text.chars();
                    let first = chars.next().unwrap_or_default();
                    if chars.next().is_none() {
                        first.to_ascii_lowercase()
                    } else {
                        '\0'
                    }
                });
            translated.set_utf8(Some(text.as_str()));
            if base != '\0' {
                translated.set_unshifted_codepoint(base);
            }
            translated.set_consumed_mods(mods & key::Mods::SHIFT);
        }
        translated.set_mods(mods);
        let mut bytes = Vec::with_capacity(128);
        // Do not discard releases here: Ghostty suppresses them in legacy mode
        // but emits them when the child requests Kitty event reporting.
        self.keyboard.encode_to_vec(&translated, &mut bytes)?;
        Ok(bytes)
    }

    pub fn mouse(&mut self, terminal: &Terminal<'_, '_>, event: &MouseEvent) -> Result<Vec<u8>> {
        let width = u32::from(self.cell_width);
        let height = u32::from(self.cell_height);
        let held = match event.action {
            MouseAction::Press => event.button.is_some_and(|button| !button.is_wheel()),
            MouseAction::Motion => event.button.is_some(),
            MouseAction::Release => false,
        };
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
            .set_any_button_pressed(held);
        let action = match event.action {
            MouseAction::Press => mouse::Action::Press,
            MouseAction::Release => mouse::Action::Release,
            MouseAction::Motion => mouse::Action::Motion,
        };
        let mut translated = mouse::Event::new()?;
        translated
            .set_action(action)
            .set_button(event.button.map(mouse_button))
            .set_mods(modifiers(event.mods))
            .set_position(mouse::Position {
                x: f32::from(event.x) * f32::from(self.cell_width),
                y: f32::from(event.y) * f32::from(self.cell_height),
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

fn modifiers(mods: Modifiers) -> key::Mods {
    let mut result = key::Mods::empty();
    for (host, child) in [
        (mods.shift, key::Mods::SHIFT),
        (mods.ctrl, key::Mods::CTRL),
        (mods.alt, key::Mods::ALT),
        (mods.meta, key::Mods::SUPER),
        (mods.caps_lock, key::Mods::CAPS_LOCK),
        (mods.num_lock, key::Mods::NUM_LOCK),
    ] {
        result.set(child, host);
    }
    result
}

fn mouse_button(button: MouseButton) -> mouse::Button {
    match button {
        MouseButton::Left => mouse::Button::Left,
        MouseButton::Middle => mouse::Button::Middle,
        MouseButton::Right => mouse::Button::Right,
        MouseButton::WheelUp => mouse::Button::Four,
        MouseButton::WheelDown => mouse::Button::Five,
        MouseButton::WheelLeft => mouse::Button::Six,
        MouseButton::WheelRight => mouse::Button::Seven,
        MouseButton::Back => mouse::Button::Eight,
        MouseButton::Forward => mouse::Button::Nine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Key;

    fn terminal() -> Terminal<'static, 'static> {
        Terminal::new(80, 24).unwrap()
    }

    const CTRL: Modifiers = Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    };

    #[test]
    fn text_control_alt_and_release() -> Result<()> {
        let terminal = terminal();
        let mut input = Input::new()?;
        let alt = Modifiers {
            alt: true,
            ..Modifiers::NONE
        };
        for (event, expected) in [
            (KeyEvent::text('c').with_mods(CTRL), b"\x03".as_slice()),
            (
                KeyEvent::text('c').with_mods(Modifiers { alt: true, ..CTRL }),
                b"\x1b\x03",
            ),
            (KeyEvent::text('x').with_mods(alt), b"\x1bx"),
            (KeyEvent::text('é'), "é".as_bytes()),
            (KeyEvent::text('🦀'), "🦀".as_bytes()),
            (KeyEvent::text('A'), b"A"),
            (KeyEvent::text(' ').with_mods(CTRL), b"\0"),
            (KeyEvent::press(Key::Enter), b"\r"),
            (KeyEvent::press(Key::Backspace), b"\x7f"),
            (KeyEvent::press(Key::ArrowLeft), b"\x1b[D"),
            (
                KeyEvent::press(Key::Tab).with_mods(Modifiers {
                    shift: true,
                    ..Modifiers::NONE
                }),
                b"\x1b[Z",
            ),
        ] {
            assert_eq!(input.key(&terminal, &event)?, expected, "{event:?}");
        }
        // Control text is never handed to Ghostty; the key and Ctrl encode it.
        let raw = KeyEvent {
            text: Some("\x03".into()),
            ..KeyEvent::press(Key::KeyC).with_mods(CTRL)
        };
        assert_eq!(input.key(&terminal, &raw)?, b"\x03");
        let release = KeyEvent::text('c')
            .with_mods(CTRL)
            .with_action(KeyAction::Release);
        assert!(input.key(&terminal, &release)?.is_empty());
        Ok(())
    }

    #[test]
    fn cursor_and_kitty_modes_are_refreshed() -> Result<()> {
        let mut terminal = terminal();
        let mut input = Input::new()?;
        let up = KeyEvent::press(Key::ArrowUp);
        assert_eq!(input.key(&terminal, &up)?, b"\x1b[A");
        terminal.vt_write(b"\x1b[?1h");
        assert_eq!(input.key(&terminal, &up)?, b"\x1bOA");
        terminal.vt_write(b"\x1b[?1l\x1b[>3u");
        let ctrl_c = KeyEvent::text('c').with_mods(CTRL);
        assert_eq!(input.key(&terminal, &ctrl_c)?, b"\x1b[99;5u");
        assert_eq!(
            input.key(&terminal, &ctrl_c.clone().with_action(KeyAction::Release))?,
            b"\x1b[99;5:3u"
        );
        terminal.vt_write(b"\x1b[<u");
        assert_eq!(input.key(&terminal, &ctrl_c)?, b"\x03");
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
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            x: 4,
            y: 2,
            mods: Modifiers::NONE,
        };
        assert!(input.mouse(&terminal, &event)?.is_empty());
        terminal.vt_write(b"\x1b[?1000h\x1b[?1006h");
        assert_eq!(input.mouse(&terminal, &event)?, b"\x1b[<0;5;3M");
        event.action = MouseAction::Release;
        assert_eq!(input.mouse(&terminal, &event)?, b"\x1b[<0;5;3m");
        event.action = MouseAction::Motion;
        event.button = None;
        assert!(input.mouse(&terminal, &event)?.is_empty());
        terminal.vt_write(b"\x1b[?1002h");
        assert!(input.mouse(&terminal, &event)?.is_empty());
        event.button = Some(MouseButton::Left);
        assert_eq!(input.mouse(&terminal, &event)?, b"\x1b[<32;5;3M");
        event.action = MouseAction::Press;
        for (button, expected) in [
            (MouseButton::WheelUp, b"\x1b[<64;5;3M".as_slice()),
            (MouseButton::WheelDown, b"\x1b[<65;5;3M"),
            (MouseButton::WheelLeft, b"\x1b[<66;5;3M"),
            (MouseButton::WheelRight, b"\x1b[<67;5;3M"),
        ] {
            event.button = Some(button);
            assert_eq!(input.mouse(&terminal, &event)?, expected);
        }
        input.set_cell_size(8, 16);
        terminal.resize(120, 40, 8, 16)?;
        event.button = Some(MouseButton::Right);
        event.x = 119;
        event.y = 39;
        event.mods = CTRL;
        assert_eq!(input.mouse(&terminal, &event)?, b"\x1b[<18;120;40M");
        terminal.vt_write(b"\x1b[?1002l");
        assert!(input.mouse(&terminal, &event)?.is_empty());
        Ok(())
    }
}
