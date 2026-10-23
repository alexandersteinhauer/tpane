//! The terminal core of tpane: a PTY with a login shell, the Ghostty terminal
//! emulator, input encoding, and rendering, without any assumption about the
//! host terminal or UI.
//!
//! # Threading model
//!
//! Ghostty objects are `!Send` and `!Sync`. Everything that touches them
//! ([`engine::Engine`], [`render::Renderer`], [`input::Input`],
//! [`pane::Pane`]) must stay on the thread that created it. The PTY reader
//! and writer ([`transport`]) and process-exit notification
//! ([`process_watch`]) are `Send` Tokio tasks that run on the runtime's worker
//! threads and talk to the owner thread through [`bus`] channels.
//!
//! The owner thread drives a future with `Runtime::block_on` or
//! `Handle::block_on`; it must not be a worker of a current-thread runtime,
//! because the I/O tasks need a worker that is free while the owner parses
//! output or renders.

pub mod bus;
pub mod engine;
pub mod event;
pub mod frame;
pub mod input;
pub mod pane;
mod process;
pub mod process_watch;
pub mod pty;
pub mod render;
pub mod selection;
pub mod transport;

/// Re-export of the pinned Ghostty bindings, for hosts and tests that need
/// direct access to the emulator.
pub use libghostty_vt as vt;

pub use event::{
    InputEvent, Key, KeyAction, KeyEvent, Modifiers, MouseAction, MouseButton, MouseEvent, Size,
};
pub use frame::{Frame, ScreenModel};
pub use pty::Command;

/// Identifies a pane on the bus.
pub type Id = u64;
