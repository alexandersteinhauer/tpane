//! Host-independent input events.
//!
//! The terminal core never sees host-specific event types (Crossterm, DOM
//! events, ...). A host converts its events into these, and
//! [`Input`](crate::input::Input) encodes them for the child according to the
//! terminal's current modes. With the `serde` feature they (de)serialize in a
//! JSON-friendly shape, e.g. `{"type":"key","key":"KeyA","text":"a"}`.

use std::{fmt, str::FromStr};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// One input event for a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "type", rename_all = "snake_case"))]
pub enum InputEvent {
    Key(KeyEvent),
    Mouse(MouseEvent),
    /// Pasted text; bracketed when the child enabled bracketed paste.
    Paste {
        text: String,
    },
    /// The viewer gained or lost focus; reported when the child asked for it.
    Focus {
        focused: bool,
    },
    /// The viewer's grid size changed.
    Resize(Size),
}

/// Terminal grid size, plus the cell size in pixels when known (0 otherwise).
/// Pixel sizes feed size reports and SGR-pixel mouse reporting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
    #[cfg_attr(feature = "serde", serde(default))]
    pub cell_width: u16,
    #[cfg_attr(feature = "serde", serde(default))]
    pub cell_height: u16,
}

impl Size {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cell_width: 0,
            cell_height: 0,
        }
    }
}

/// Modifier state. `meta` is the Super/Command/Windows key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Modifiers {
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub shift: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub ctrl: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub alt: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub meta: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub caps_lock: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub num_lock: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
        caps_lock: false,
        num_lock: false,
    };
}

#[cfg(feature = "serde")]
pub(crate) fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum KeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

/// A key event, modelled on the DOM `KeyboardEvent`.
///
/// `key` is the physical key (`KeyboardEvent.code`). `text` is the text the
/// key produced with the current layout and modifiers (`KeyboardEvent.key`
/// when it is a printable character), without control-character
/// transformation: Ctrl+C is `key: KeyC, text: "c", ctrl`. Text containing
/// control characters is ignored; the encoder performs Ctrl transformation.
/// `unshifted` is the character the key produces without modifiers, used by
/// the Kitty keyboard protocol; when absent it is derived from `key` for a
/// US layout.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct KeyEvent {
    pub key: Key,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub text: Option<String>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub unshifted: Option<char>,
    #[cfg_attr(feature = "serde", serde(default))]
    pub mods: Modifiers,
    #[cfg_attr(feature = "serde", serde(default))]
    pub action: KeyAction,
}

impl KeyEvent {
    /// A key press without text, e.g. `Enter` or `ArrowUp`.
    pub fn press(key: Key) -> Self {
        Self {
            key,
            ..Self::default()
        }
    }

    /// A key press producing `text` on a US layout. Uppercase ASCII letters
    /// and shifted symbols imply Shift.
    pub fn text(ch: char) -> Self {
        let key = Key::from_us_char(ch);
        let shift = ch.is_ascii_uppercase() || key.us_char().is_some_and(|base| base != ch);
        Self {
            key,
            text: Some(ch.to_string()),
            unshifted: None,
            mods: Modifiers {
                shift,
                ..Modifiers::NONE
            },
            action: KeyAction::Press,
        }
    }

    pub fn with_mods(mut self, mods: Modifiers) -> Self {
        self.mods = mods;
        self
    }

    pub fn with_action(mut self, action: KeyAction) -> Self {
        self.action = action;
        self
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
    Back,
    Forward,
}

impl MouseButton {
    pub fn is_wheel(self) -> bool {
        matches!(
            self,
            Self::WheelUp | Self::WheelDown | Self::WheelLeft | Self::WheelRight
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MouseAction {
    Press,
    Release,
    /// Movement; `button` is the held button while dragging, else `None`.
    Motion,
}

/// A mouse event at a cell of the terminal's viewport (zero-based). Wheel
/// steps are presses of the wheel buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct MouseEvent {
    pub action: MouseAction,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub button: Option<MouseButton>,
    pub x: u16,
    pub y: u16,
    #[cfg_attr(feature = "serde", serde(default))]
    pub mods: Modifiers,
}

macro_rules! keys {
    ($($name:ident => $ghostty:ident,)*) => {
        /// A physical key, named like the DOM `KeyboardEvent.code` values
        /// (`KeyA`, `Digit1`, `ArrowUp`, `NumpadEnter`, `F12`, ...). The set is
        /// Ghostty's key set, so a few Ghostty-only names (`NumpadUp`, ...)
        /// exist too. Unknown names parse as `Unidentified`.
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
        pub enum Key {
            #[default]
            Unidentified,
            $($name,)*
        }

        impl Key {
            pub fn as_str(self) -> &'static str {
                match self {
                    Self::Unidentified => "Unidentified",
                    $(Self::$name => stringify!($name),)*
                }
            }

            pub(crate) fn to_ghostty(self) -> libghostty_vt::key::Key {
                use libghostty_vt::key::Key as G;
                match self {
                    Self::Unidentified => G::Unidentified,
                    $(Self::$name => G::$ghostty,)*
                }
            }
        }

        impl FromStr for Key {
            type Err = std::convert::Infallible;

            fn from_str(name: &str) -> Result<Self, Self::Err> {
                Ok(match name {
                    $(stringify!($name) => Self::$name,)*
                    _ => Self::Unidentified,
                })
            }
        }
    };
}

keys! {
Backquote => Backquote,
Backslash => Backslash,
BracketLeft => BracketLeft,
BracketRight => BracketRight,
Comma => Comma,
Digit0 => Digit0,
Digit1 => Digit1,
Digit2 => Digit2,
Digit3 => Digit3,
Digit4 => Digit4,
Digit5 => Digit5,
Digit6 => Digit6,
Digit7 => Digit7,
Digit8 => Digit8,
Digit9 => Digit9,
Equal => Equal,
IntlBackslash => IntlBackslash,
IntlRo => IntlRo,
IntlYen => IntlYen,
KeyA => A,
KeyB => B,
KeyC => C,
KeyD => D,
KeyE => E,
KeyF => F,
KeyG => G,
KeyH => H,
KeyI => I,
KeyJ => J,
KeyK => K,
KeyL => L,
KeyM => M,
KeyN => N,
KeyO => O,
KeyP => P,
KeyQ => Q,
KeyR => R,
KeyS => S,
KeyT => T,
KeyU => U,
KeyV => V,
KeyW => W,
KeyX => X,
KeyY => Y,
KeyZ => Z,
Minus => Minus,
Period => Period,
Quote => Quote,
Semicolon => Semicolon,
Slash => Slash,
AltLeft => AltLeft,
AltRight => AltRight,
Backspace => Backspace,
CapsLock => CapsLock,
ContextMenu => ContextMenu,
ControlLeft => ControlLeft,
ControlRight => ControlRight,
Enter => Enter,
MetaLeft => MetaLeft,
MetaRight => MetaRight,
ShiftLeft => ShiftLeft,
ShiftRight => ShiftRight,
Space => Space,
Tab => Tab,
Convert => Convert,
KanaMode => KanaMode,
NonConvert => NonConvert,
Delete => Delete,
End => End,
Help => Help,
Home => Home,
Insert => Insert,
PageDown => PageDown,
PageUp => PageUp,
ArrowDown => ArrowDown,
ArrowLeft => ArrowLeft,
ArrowRight => ArrowRight,
ArrowUp => ArrowUp,
NumLock => NumLock,
Numpad0 => Numpad0,
Numpad1 => Numpad1,
Numpad2 => Numpad2,
Numpad3 => Numpad3,
Numpad4 => Numpad4,
Numpad5 => Numpad5,
Numpad6 => Numpad6,
Numpad7 => Numpad7,
Numpad8 => Numpad8,
Numpad9 => Numpad9,
NumpadAdd => NumpadAdd,
NumpadBackspace => NumpadBackspace,
NumpadClear => NumpadClear,
NumpadClearEntry => NumpadClearEntry,
NumpadComma => NumpadComma,
NumpadDecimal => NumpadDecimal,
NumpadDivide => NumpadDivide,
NumpadEnter => NumpadEnter,
NumpadEqual => NumpadEqual,
NumpadMemoryAdd => NumpadMemoryAdd,
NumpadMemoryClear => NumpadMemoryClear,
NumpadMemoryRecall => NumpadMemoryRecall,
NumpadMemoryStore => NumpadMemoryStore,
NumpadMemorySubtract => NumpadMemorySubtract,
NumpadMultiply => NumpadMultiply,
NumpadParenLeft => NumpadParenLeft,
NumpadParenRight => NumpadParenRight,
NumpadSubtract => NumpadSubtract,
NumpadSeparator => NumpadSeparator,
NumpadUp => NumpadUp,
NumpadDown => NumpadDown,
NumpadRight => NumpadRight,
NumpadLeft => NumpadLeft,
NumpadBegin => NumpadBegin,
NumpadHome => NumpadHome,
NumpadEnd => NumpadEnd,
NumpadInsert => NumpadInsert,
NumpadDelete => NumpadDelete,
NumpadPageUp => NumpadPageUp,
NumpadPageDown => NumpadPageDown,
Escape => Escape,
F1 => F1,
F2 => F2,
F3 => F3,
F4 => F4,
F5 => F5,
F6 => F6,
F7 => F7,
F8 => F8,
F9 => F9,
F10 => F10,
F11 => F11,
F12 => F12,
F13 => F13,
F14 => F14,
F15 => F15,
F16 => F16,
F17 => F17,
F18 => F18,
F19 => F19,
F20 => F20,
F21 => F21,
F22 => F22,
F23 => F23,
F24 => F24,
F25 => F25,
Fn => Fn,
FnLock => FnLock,
PrintScreen => PrintScreen,
ScrollLock => ScrollLock,
Pause => Pause,
BrowserBack => BrowserBack,
BrowserFavorites => BrowserFavorites,
BrowserForward => BrowserForward,
BrowserHome => BrowserHome,
BrowserRefresh => BrowserRefresh,
BrowserSearch => BrowserSearch,
BrowserStop => BrowserStop,
Eject => Eject,
LaunchApp1 => LaunchApp1,
LaunchApp2 => LaunchApp2,
LaunchMail => LaunchMail,
MediaPlayPause => MediaPlayPause,
MediaSelect => MediaSelect,
MediaStop => MediaStop,
MediaTrackNext => MediaTrackNext,
MediaTrackPrevious => MediaTrackPrevious,
Power => Power,
Sleep => Sleep,
AudioVolumeDown => AudioVolumeDown,
AudioVolumeMute => AudioVolumeMute,
AudioVolumeUp => AudioVolumeUp,
WakeUp => WakeUp,}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl Serialize for Key {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Ok(name.parse().unwrap_or_default())
    }
}

const US_KEYS: [(char, Key); 47] = [
    ('`', Key::Backquote),
    ('\\', Key::Backslash),
    ('[', Key::BracketLeft),
    (']', Key::BracketRight),
    (',', Key::Comma),
    ('0', Key::Digit0),
    ('1', Key::Digit1),
    ('2', Key::Digit2),
    ('3', Key::Digit3),
    ('4', Key::Digit4),
    ('5', Key::Digit5),
    ('6', Key::Digit6),
    ('7', Key::Digit7),
    ('8', Key::Digit8),
    ('9', Key::Digit9),
    ('=', Key::Equal),
    ('a', Key::KeyA),
    ('b', Key::KeyB),
    ('c', Key::KeyC),
    ('d', Key::KeyD),
    ('e', Key::KeyE),
    ('f', Key::KeyF),
    ('g', Key::KeyG),
    ('h', Key::KeyH),
    ('i', Key::KeyI),
    ('j', Key::KeyJ),
    ('k', Key::KeyK),
    ('l', Key::KeyL),
    ('m', Key::KeyM),
    ('n', Key::KeyN),
    ('o', Key::KeyO),
    ('p', Key::KeyP),
    ('q', Key::KeyQ),
    ('r', Key::KeyR),
    ('s', Key::KeyS),
    ('t', Key::KeyT),
    ('u', Key::KeyU),
    ('v', Key::KeyV),
    ('w', Key::KeyW),
    ('x', Key::KeyX),
    ('y', Key::KeyY),
    ('z', Key::KeyZ),
    ('-', Key::Minus),
    ('.', Key::Period),
    ('\'', Key::Quote),
    (';', Key::Semicolon),
    ('/', Key::Slash),
];

impl Key {
    /// The key that produces `ch` on a US layout, shifted or not; `Space`
    /// for a space and `Unidentified` for anything else.
    pub fn from_us_char(ch: char) -> Self {
        if ch == ' ' {
            return Self::Space;
        }
        let base = us_unshifted(ch);
        US_KEYS
            .iter()
            .find_map(|&(c, key)| (c == base).then_some(key))
            .unwrap_or(Self::Unidentified)
    }

    /// The unshifted character of this key on a US layout.
    pub fn us_char(self) -> Option<char> {
        if self == Self::Space {
            return Some(' ');
        }
        US_KEYS
            .iter()
            .find_map(|&(c, key)| (key == self).then_some(c))
    }
}

/// The unshifted ASCII counterpart of a US-layout character; other
/// characters are lowercased if ASCII and returned unchanged otherwise.
pub fn us_unshifted(ch: char) -> char {
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

/// The shifted counterpart of a US-layout ASCII character.
pub fn us_shifted(ch: char) -> char {
    const BASE: &str = "`1234567890-=[]\\;',./";
    const SHIFTED: &str = "~!@#$%^&*()_+{}|:\"<>?";
    BASE.chars()
        .zip(SHIFTED.chars())
        .find_map(|(base, shifted)| (ch == base).then_some(shifted))
        .unwrap_or_else(|| ch.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_names_round_trip_and_map_letters() {
        for key in [
            Key::KeyA,
            Key::Digit1,
            Key::ArrowUp,
            Key::NumpadEnter,
            Key::F25,
        ] {
            assert_eq!(key.as_str().parse::<Key>().unwrap(), key);
        }
        assert_eq!("KeyQ".parse::<Key>().unwrap(), Key::KeyQ);
        assert_eq!("Q".parse::<Key>().unwrap(), Key::Unidentified);
        assert_eq!(Key::KeyQ.to_ghostty(), libghostty_vt::key::Key::Q);
        assert_eq!(Key::from_us_char('Q'), Key::KeyQ);
        assert_eq!(Key::from_us_char('?'), Key::Slash);
        assert_eq!(Key::Slash.us_char(), Some('/'));
        let event = KeyEvent::text('?');
        assert!(event.mods.shift);
        assert!(!KeyEvent::text('a').mods.shift);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn events_have_a_compact_json_shape() {
        let event = InputEvent::Key(KeyEvent::text('a'));
        let json = serde_json::to_string(&event).unwrap();
        assert_eq!(
            json,
            r#"{"type":"key","key":"KeyA","text":"a","mods":{},"action":"press"}"#
        );
        let parsed: InputEvent =
            serde_json::from_str(r#"{"type":"key","key":"Enter","mods":{"ctrl":true}}"#).unwrap();
        assert_eq!(
            parsed,
            InputEvent::Key(KeyEvent::press(Key::Enter).with_mods(Modifiers {
                ctrl: true,
                ..Modifiers::NONE
            }))
        );
        let mouse: InputEvent = serde_json::from_str(
            r#"{"type":"mouse","action":"press","button":"wheel_up","x":3,"y":4}"#,
        )
        .unwrap();
        assert!(matches!(
            mouse,
            InputEvent::Mouse(MouseEvent {
                button: Some(MouseButton::WheelUp),
                x: 3,
                ..
            })
        ));
        let resize: InputEvent =
            serde_json::from_str(r#"{"type":"resize","cols":100,"rows":30}"#).unwrap();
        assert_eq!(resize, InputEvent::Resize(Size::new(100, 30)));
        let paste: InputEvent = serde_json::from_str(r#"{"type":"paste","text":"x"}"#).unwrap();
        assert_eq!(paste, InputEvent::Paste { text: "x".into() });
    }
}
