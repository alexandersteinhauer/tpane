use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use anyhow::{Result, ensure};
use libghostty_vt::{
    Terminal,
    terminal::{
        ConformanceLevel, DeviceAttributeFeature, DeviceAttributes, DeviceType,
        PrimaryDeviceAttributes, SecondaryDeviceAttributes, SizeReportSize,
        TertiaryDeviceAttributes,
    },
};

pub struct Engine {
    pub terminal: Terminal<'static, 'static>,
    replies: Rc<RefCell<Vec<u8>>>,
    cell_size: Rc<Cell<(u16, u16)>>,
}

impl Engine {
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        let mut terminal = Terminal::new(cols, rows)?;
        // Ghostty limits scrollback by bytes, pruning whole pages.
        terminal.set_scrollback_max_bytes(Some(10_000))?;
        let replies = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&replies);
        terminal.on_pty_write(move |_, bytes| sink.borrow_mut().extend_from_slice(bytes))?;
        terminal.on_xtversion(|_| Some("tpane 0.1.0"))?;
        terminal.on_device_attributes(|_| {
            Some(DeviceAttributes {
                primary: PrimaryDeviceAttributes::new(
                    ConformanceLevel::VT220,
                    &[DeviceAttributeFeature::ANSI_COLOR],
                ),
                secondary: SecondaryDeviceAttributes {
                    device_type: DeviceType::VT220,
                    firmware_version: 1,
                    rom_cartridge: 0,
                },
                tertiary: TertiaryDeviceAttributes::default(),
            })
        })?;
        let cell_size = Rc::new(Cell::new((0, 0)));
        let geometry = Rc::clone(&cell_size);
        terminal.on_size(move |terminal| {
            Some(SizeReportSize {
                rows: terminal.rows().ok()?,
                columns: terminal.cols().ok()?,
                cell_width: u32::from(geometry.get().0),
                cell_height: u32::from(geometry.get().1),
            })
        })?;
        terminal.set_apc_max_bytes(Some(1024 * 1024))?;
        Ok(Self {
            terminal,
            replies,
            cell_size,
        })
    }

    pub fn resize(&mut self, cols: u16, rows: u16, width: u16, height: u16) -> Result<()> {
        self.terminal
            .resize(cols, rows, u32::from(width), u32::from(height))?;
        self.cell_size.set((width, height));
        Ok(())
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        // One bus event is one feed; the transport caps the event size.
        ensure!(
            bytes.len() <= crate::transport::CHUNK,
            "PTY output chunk exceeds processing budget"
        );
        self.terminal.vt_write(bytes);
        Ok(())
    }

    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.replies.borrow_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::{screen::Screen, terminal::ScrollViewport};

    #[test]
    fn answers_cursor_and_device_queries_without_contacting_host() {
        let mut engine = Engine::new(80, 24).unwrap();
        engine.feed(b"abc\x1b[6n").unwrap();
        assert_eq!(engine.take_replies(), b"\x1b[1;4R");
        engine.feed(b"\x1b[c").unwrap();
        assert!(!engine.take_replies().is_empty());
        engine.feed(b"\x1b[18t").unwrap();
        assert_eq!(engine.take_replies(), b"\x1b[8;24;80t");
    }

    #[test]
    fn resize_emits_in_band_reports_without_more_pty_output() {
        let mut engine = Engine::new(80, 24).unwrap();
        engine.feed(b"\x1b[?2048h").unwrap();
        // Enabling mode 2048 reports the current size immediately.
        assert_eq!(engine.take_replies(), b"\x1b[48;24;80;0;0t");
        engine.resize(101, 31, 9, 18).unwrap();
        assert_eq!(engine.take_replies(), b"\x1b[48;31;101;558;909t");
        engine.resize(47, 12, 9, 18).unwrap();
        assert_eq!(engine.take_replies(), b"\x1b[48;12;47;216;423t");
        assert!(engine.take_replies().is_empty());
    }

    #[test]
    fn alternate_screen_restores_shell_and_resize_preserves_history() {
        let mut engine = Engine::new(10, 2).unwrap();
        engine.feed(b"shell\r\nline two\r\nline three").unwrap();
        assert!(engine.terminal.scrollback_rows().unwrap() > 0);
        engine.feed(b"\x1b[?1049heditor").unwrap();
        assert_eq!(engine.terminal.active_screen().unwrap(), Screen::Alternate);
        engine.feed(b"\x1b[?1049l").unwrap();
        assert_eq!(engine.terminal.active_screen().unwrap(), Screen::Primary);
        engine.terminal.resize(20, 4, 0, 0).unwrap();
        engine.terminal.scroll_viewport(ScrollViewport::Top);
        assert_eq!(engine.terminal.cols().unwrap(), 20);
    }
}
