use std::io::Write;

use anyhow::Result;
use libghostty_vt::{
    RenderState, Terminal,
    render::{CellIterator, CursorVisualStyle, Dirty, RowIterator},
    screen::CellWide,
    style::{RgbColor, Style, StyleColor, Underline},
    terminal::Mode,
};

pub struct Renderer {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    buffer: Vec<u8>,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            buffer: Vec::new(),
        })
    }

    #[cfg(test)]
    pub fn draw(
        &mut self,
        terminal: &Terminal<'static, '_>,
        output: &mut impl Write,
    ) -> Result<()> {
        let mut frame = Vec::new();
        self.draw_at(terminal, &mut frame, 0, false)?;
        if !frame.is_empty() {
            frame.extend_from_slice(b"\x1b[?2026l");
            output.write_all(&frame)?;
        }
        Ok(())
    }

    pub fn draw_at(
        &mut self,
        terminal: &Terminal<'static, '_>,
        output: &mut impl Write,
        top: u16,
        force: bool,
    ) -> Result<()> {
        let snapshot = self.state.update(terminal)?;
        let dirty = if force {
            Dirty::Full
        } else {
            snapshot.dirty()?
        };
        if dirty == Dirty::Clean {
            return Ok(());
        }
        self.buffer.clear();
        self.buffer.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        let colors = snapshot.colors()?;
        let default_fg = terminal.fg_color()?;
        let default_bg = terminal.bg_color()?;
        let reverse = terminal.mode(Mode::REVERSE_COLORS)?;
        let mut rows = self.rows.update(&snapshot)?;
        let mut y = 0;
        let mut text = String::new();
        while let Some(row) = rows.next() {
            if dirty == Dirty::Full || row.dirty()? {
                write!(self.buffer, "\x1b[{};1H", u32::from(top) + y + 1)?;
                let mut cells = self.cells.update(row)?;
                let mut previous = None;
                while let Some(cell) = cells.next() {
                    let wide = cell.raw_cell()?.wide()?;
                    if wide == CellWide::SpacerTail {
                        continue;
                    }
                    let mut style = cell.style()?;
                    if let Some(color) = cell.fg_color()?.or(default_fg) {
                        style.fg_color = StyleColor::Rgb(color);
                    }
                    if let Some(color) = cell.bg_color()?.or(default_bg) {
                        style.bg_color = StyleColor::Rgb(color);
                    }
                    style.inverse ^= reverse ^ cell.is_selected()?;
                    if previous != Some(style) {
                        emit_style(&mut self.buffer, style, &colors.palette)?;
                        previous = Some(style);
                    }
                    text.clear();
                    cell.graphemes_utf8(&mut text)?;
                    if text.is_empty() || wide == CellWide::SpacerHead {
                        self.buffer.push(b' ');
                    } else {
                        self.buffer.extend_from_slice(text.as_bytes());
                    }
                }
                row.set_dirty(false)?;
            }
            y += 1;
        }
        self.buffer.extend_from_slice(b"\x1b[0m");
        if snapshot.cursor_visible()?
            && let Some(cursor) = snapshot.cursor_viewport()?
        {
            let base = match snapshot.cursor_visual_style()? {
                CursorVisualStyle::Underline => 3,
                CursorVisualStyle::Bar => 5,
                _ => 1,
            };
            let shape = base + u8::from(!snapshot.cursor_blinking()?);
            write!(
                self.buffer,
                "\x1b[{};{}H\x1b[{} q\x1b[?25h",
                u32::from(top) + u32::from(cursor.y) + 1,
                cursor.x + 1,
                shape
            )?;
        }
        // App commits synchronized output only after composing its overlay.
        output.write_all(&self.buffer)?;
        output.flush()?;
        snapshot.set_dirty(Dirty::Clean)?;
        Ok(())
    }
}

fn emit_style(output: &mut impl Write, style: Style, palette: &[RgbColor; 256]) -> Result<()> {
    output.write_all(b"\x1b[0m")?;
    for (enabled, code) in [
        (style.bold, 1),
        (style.faint, 2),
        (style.italic, 3),
        (style.blink, 5),
        (style.inverse, 7),
        (style.invisible, 8),
        (style.strikethrough, 9),
        (style.overline, 53),
    ] {
        if enabled {
            write!(output, "\x1b[{code}m")?;
        }
    }
    let underline = match style.underline {
        Underline::None => 0,
        Underline::Single => 1,
        Underline::Double => 2,
        Underline::Curly => 3,
        Underline::Dotted => 4,
        Underline::Dashed => 5,
        _ => 1,
    };
    if underline != 0 {
        write!(output, "\x1b[4:{underline}m")?;
    }
    for (color, code) in [
        (style.fg_color, 38),
        (style.bg_color, 48),
        (style.underline_color, 58),
    ] {
        let rgb = match color {
            StyleColor::None => continue,
            StyleColor::Palette(index) => palette[usize::from(index.0)],
            StyleColor::Rgb(rgb) => rgb,
        };
        write!(output, "\x1b[{code};2;{};{};{}m", rgb.r, rgb.g, rgb.b)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::{
        TerminalOptions,
        terminal::{Point, PointCoordinate},
    };

    fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
        Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: 100,
        })
        .unwrap()
    }

    fn text_at(terminal: &Terminal<'_, '_>, x: u16, y: u32) -> u32 {
        terminal
            .grid_ref(Point::Active(PointCoordinate { x, y }))
            .unwrap()
            .cell()
            .unwrap()
            .codepoint()
            .unwrap()
    }

    #[test]
    fn renders_wide_combining_and_colored_text_without_shifting_columns() {
        let mut source = terminal(12, 3);
        source.vt_write("\x1b[31m界e\u{301}Z\x1b[0m".as_bytes());
        let mut output = Vec::new();
        Renderer::new().unwrap().draw(&source, &mut output).unwrap();
        let mut host = terminal(12, 3);
        host.vt_write(b"\x1b[?7l");
        host.vt_write(&output);
        assert_eq!(text_at(&host, 0, 0), '界' as u32);
        assert_eq!(text_at(&host, 2, 0), 'e' as u32);
        assert_eq!(text_at(&host, 3, 0), 'Z' as u32);
        assert_eq!(host.cursor_x().unwrap(), source.cursor_x().unwrap());
    }

    #[test]
    fn renders_erased_background_cells_and_dynamic_default_colors() {
        let mut source = terminal(8, 2);
        source.vt_write(b"\x1b]10;rgb:ff/00/00\x1b\\\x1b[44m\x1b[2J\x1b[0mX");
        let mut output = Vec::new();
        Renderer::new().unwrap().draw(&source, &mut output).unwrap();
        let mut host = terminal(8, 2);
        host.vt_write(b"\x1b[?7l");
        host.vt_write(&output);
        let first = host
            .grid_ref(Point::Active(PointCoordinate { x: 0, y: 0 }))
            .unwrap()
            .style()
            .unwrap();
        assert_eq!(
            first.fg_color,
            StyleColor::Rgb(RgbColor { r: 255, g: 0, b: 0 })
        );
        let mut state = RenderState::new().unwrap();
        let snapshot = state.update(&host).unwrap();
        let mut rows = RowIterator::new().unwrap();
        let mut cells = CellIterator::new().unwrap();
        let mut rows = rows.update(&snapshot).unwrap();
        let mut cells = cells.update(rows.next().unwrap()).unwrap();
        cells.select(3).unwrap();
        assert_eq!(
            cells.bg_color().unwrap(),
            Some(source.color_palette().unwrap().0[4])
        );
    }

    #[test]
    fn offset_and_forced_redraw_preserve_ancestor_bars() {
        let mut source = terminal(12, 2);
        source.vt_write(b"pane text");
        let mut renderer = Renderer::new().unwrap();
        let mut output = Vec::new();
        renderer.draw_at(&source, &mut output, 2, false).unwrap();
        let mut host = terminal(12, 4);
        host.vt_write(b"\x1b[?7lroot bar\r\nchild bar");
        host.vt_write(&output);
        assert_eq!(text_at(&host, 0, 0), 'r' as u32);
        assert_eq!(text_at(&host, 0, 1), 'c' as u32);
        assert_eq!(text_at(&host, 0, 2), 'p' as u32);
        assert_eq!(host.cursor_y().unwrap(), 2);
        host.vt_write(b"\x1b[3;1Hother pane");
        output.clear();
        renderer.draw_at(&source, &mut output, 2, true).unwrap();
        host.vt_write(&output);
        assert_eq!(text_at(&host, 0, 2), 'p' as u32);
        assert_eq!(text_at(&host, 0, 0), 'r' as u32);
    }

    #[test]
    fn does_not_scroll_when_painting_bottom_right_and_skips_clean_frames() {
        let mut source = terminal(4, 2);
        source.vt_write(b"abcd\r\nefgh");
        let mut renderer = Renderer::new().unwrap();
        let mut output = Vec::new();
        renderer.draw(&source, &mut output).unwrap();
        let mut host = terminal(4, 2);
        host.vt_write(b"\x1b[?7l");
        host.vt_write(&output);
        assert_eq!(text_at(&host, 0, 0), 'a' as u32);
        assert_eq!(text_at(&host, 3, 1), 'h' as u32);
        output.clear();
        renderer.draw(&source, &mut output).unwrap();
        assert!(output.is_empty());
    }
}
