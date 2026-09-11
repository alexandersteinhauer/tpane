use std::io::Write;

use anyhow::Result;
use libghostty_vt::{
    RenderState, Terminal,
    error::Error,
    render::{CellIterator, CursorVisualStyle, Dirty, RowIterator},
    screen::CellWide,
    style::{Id as StyleId, RgbColor, Style, StyleColor, Underline},
    terminal::Mode,
};

pub struct Renderer {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    buffer: Vec<u8>,
    // Reused grapheme scratch space: cleared per cell, allocated once.
    text: String,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            state: RenderState::new()?,
            rows: RowIterator::new()?,
            cells: CellIterator::new()?,
            buffer: Vec::new(),
            text: String::new(),
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
        // Cells without explicit styling share one base style: the terminal
        // defaults with no attributes. Explicit fg/bg colors are only read when
        // a cell is styled or carries a content background, because emit_style
        // resolves palette indices itself.
        let mut base = Style::default();
        if let Some(color) = default_fg {
            base.fg_color = StyleColor::Rgb(color);
        }
        if let Some(color) = default_bg {
            base.bg_color = StyleColor::Rgb(color);
        }
        let mut rows = self.rows.update(&snapshot)?;
        let mut y = 0;
        // The pen survives the absolute row-positioning writes, so a style is
        // only emitted when it changes, and it can stay across rows. Every
        // frame ends with a reset, so each call starts from an unknown pen.
        let mut previous: Option<Style> = None;
        let mut cached_style_id: Option<StyleId> = None;
        let mut cached_style = Style::default();
        while let Some(row) = rows.next() {
            if dirty == Dirty::Full || row.dirty()? {
                write!(self.buffer, "\x1b[{};1H", u32::from(top) + y + 1)?;
                let raw_row = row.raw_row()?;
                // Row-level flags can have false positives but never miss
                // styling or graphemes, so false is a safe fast path.
                let row_styled = raw_row.is_styled()?;
                let row_graphemes = raw_row.has_grapheme_cluster()?;
                let selection = row.selection()?;
                let mut cells = self.cells.update(row)?;
                let mut x: u16 = 0;
                // A wide cell is always followed by its spacer tail, so the
                // tail needs no queries at all.
                let mut skip_tail = false;
                while let Some(cell) = cells.next() {
                    if skip_tail {
                        skip_tail = false;
                        x += 1;
                        continue;
                    }
                    let raw = cell.raw_cell()?;
                    let wide = raw.wide()?;
                    // Spacer tails cannot start a row, so this only guards
                    // against inconsistent row flags.
                    if wide == CellWide::SpacerTail {
                        x += 1;
                        continue;
                    }
                    let selected = selection
                        .as_ref()
                        .is_some_and(|range| x >= range.start_x && x <= range.end_x);
                    // Text first: a cell without text may carry a background
                    // color in its content, which changes the style below.
                    let codepoint = if row_graphemes {
                        self.text.clear();
                        cell.graphemes_utf8(&mut self.text)?;
                        None
                    } else {
                        // No cell in this row has grapheme clusters, so the
                        // single codepoint is the whole cell text.
                        Some(raw.codepoint()?)
                    };
                    let has_text = codepoint.map_or(!self.text.is_empty(), |cp| cp != 0);
                    let mut style = if row_styled {
                        // Runs of cells share one style id, so the full style
                        // and its color lookups are only read on change.
                        let id = raw.style_id()?;
                        if cached_style_id != Some(id) {
                            cached_style = cell.style()?;
                            cached_style_id = Some(id);
                        }
                        cached_style
                    } else {
                        base
                    };
                    if style.fg_color == StyleColor::None {
                        style.fg_color = base.fg_color;
                    }
                    if has_text {
                        if style.bg_color == StyleColor::None {
                            style.bg_color = base.bg_color;
                        }
                    } else if let Some(color) = cell.bg_color()?.or(default_bg) {
                        style.bg_color = StyleColor::Rgb(color);
                    }
                    style.inverse ^= reverse ^ selected;
                    if previous != Some(style) {
                        emit_style(&mut self.buffer, style, &colors.palette)?;
                        previous = Some(style);
                    }
                    if !has_text || wide == CellWide::SpacerHead {
                        self.buffer.push(b' ');
                    } else if let Some(codepoint) = codepoint {
                        let character = char::from_u32(codepoint).ok_or(Error::InvalidValue)?;
                        let mut encoded = [0; 4];
                        self.buffer
                            .extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
                    } else {
                        self.buffer.extend_from_slice(self.text.as_bytes());
                    }
                    if wide == CellWide::Wide {
                        skip_tail = true;
                    }
                    x += 1;
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

    /// The pre-optimization renderer, kept only as a differential-test
    /// reference: every cell queries its style, colors, selection, and
    /// graphemes independently, and the pen resets at every row.
    #[cfg(test)]
    fn draw_reference_at(
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
        selection::Selection,
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

    fn style_at(terminal: &Terminal<'_, '_>, x: u16, y: u32) -> Style {
        terminal
            .grid_ref(Point::Active(PointCoordinate { x, y }))
            .unwrap()
            .style()
            .unwrap()
    }

    /// Full visible cell state plus cursor position, for differential tests.
    fn grid_snapshot(
        terminal: &Terminal<'_, '_>,
        cols: u16,
        rows: u16,
    ) -> (Vec<(u32, CellWide, Style)>, u16, u16) {
        let mut cells = Vec::with_capacity(usize::from(cols) * usize::from(rows));
        for y in 0..u32::from(rows) {
            for x in 0..cols {
                let grid_ref = terminal
                    .grid_ref(Point::Active(PointCoordinate { x, y }))
                    .unwrap();
                let cell = grid_ref.cell().unwrap();
                cells.push((
                    cell.codepoint().unwrap(),
                    cell.wide().unwrap(),
                    grid_ref.style().unwrap(),
                ));
            }
        }
        (
            cells,
            terminal.cursor_x().unwrap(),
            terminal.cursor_y().unwrap(),
        )
    }

    #[test]
    fn selection_inverts_only_the_selected_cells() {
        let mut source = terminal(8, 2);
        source.vt_write(b"abcdefgh\r\nijklmnop");
        let start = source
            .grid_ref(Point::Active(PointCoordinate { x: 2, y: 0 }))
            .unwrap();
        let end = source
            .grid_ref(Point::Active(PointCoordinate { x: 4, y: 0 }))
            .unwrap();
        let selection = Selection::new(start, end, false);
        source.set_selection(Some(&selection)).unwrap();
        let mut output = Vec::new();
        Renderer::new().unwrap().draw(&source, &mut output).unwrap();
        let mut host = terminal(8, 2);
        host.vt_write(b"\x1b[?7l");
        host.vt_write(&output);
        for x in 0..8 {
            assert_eq!(style_at(&host, x, 0).inverse, (2..=4).contains(&x), "x={x}");
        }
        // The second row does not intersect the selection.
        for x in 0..8 {
            assert!(!style_at(&host, x, 1).inverse, "x={x}");
        }
    }

    #[test]
    fn renders_wide_characters_at_the_wrap_boundary() {
        let mut source = terminal(6, 3);
        source.vt_write("01234界Z".as_bytes());
        let mut output = Vec::new();
        Renderer::new().unwrap().draw(&source, &mut output).unwrap();
        let mut host = terminal(6, 3);
        host.vt_write(b"\x1b[?7l");
        host.vt_write(&output);
        // The wide character does not fit in the first row, so it moves to the
        // continuation row and the last column becomes a blank spacer.
        assert_eq!(text_at(&host, 0, 1), '界' as u32);
        assert_eq!(text_at(&host, 2, 1), 'Z' as u32);
        assert_eq!(text_at(&host, 5, 0), ' ' as u32);
        assert_eq!(host.cursor_x().unwrap(), source.cursor_x().unwrap());
    }

    #[test]
    fn reuses_one_style_sequence_across_rows_with_the_same_style() {
        let mut source = terminal(4, 2);
        source.vt_write(b"\x1b]10;rgb:ff/80/00\x1b\\\x1b]11;rgb:00/40/ff\x1b\\ab\r\ncd");
        let mut output = Vec::new();
        Renderer::new().unwrap().draw(&source, &mut output).unwrap();
        // One style emission for the first cell plus the frame-end reset; the
        // second row must not re-emit the same colors.
        assert_eq!(
            output.windows(4).filter(|w| *w == &b"\x1b[0m"[..]).count(),
            2
        );
        assert_eq!(
            output
                .windows(7)
                .filter(|w| *w == &b"\x1b[38;2;"[..])
                .count(),
            1
        );
    }

    #[test]
    #[ignore = "diagnostic timing, not an assertion"]
    fn microbench_render_paths() {
        use std::time::Instant;
        const COLS: u16 = 100;
        const ROWS: u16 = 28;
        const FRAMES: u32 = 200;
        let mut source = terminal(COLS, ROWS);
        for _ in 0..ROWS {
            source.vt_write(
                b"2025-01-01T12:34:56 INFO request=012345 status=200 bytes=4096        \r\n",
            );
        }
        let mut new = Renderer::new().unwrap();
        let mut reference = Renderer::new().unwrap();
        let mut out_new = Vec::new();
        let mut out_ref = Vec::new();
        for _ in 0..5 {
            out_new.clear();
            new.draw_at(&source, &mut out_new, 0, true).unwrap();
            out_ref.clear();
            reference
                .draw_reference_at(&source, &mut out_ref, 0, true)
                .unwrap();
        }
        let start = Instant::now();
        for _ in 0..FRAMES {
            out_new.clear();
            new.draw_at(&source, &mut out_new, 0, true).unwrap();
        }
        let new_time = start.elapsed();
        let start = Instant::now();
        for _ in 0..FRAMES {
            out_ref.clear();
            reference
                .draw_reference_at(&source, &mut out_ref, 0, true)
                .unwrap();
        }
        let reference_time = start.elapsed();
        println!(
            "new {:.1} us/frame ({} bytes), reference {:.1} us/frame ({} bytes)",
            new_time.as_secs_f64() * 1e6 / f64::from(FRAMES),
            out_new.len(),
            reference_time.as_secs_f64() * 1e6 / f64::from(FRAMES),
            out_ref.len()
        );
    }

    #[test]
    fn matches_the_reference_renderer_across_frames_and_features() {
        const COLS: u16 = 12;
        const ROWS: u16 = 6;
        let steps: &[&[u8]] = &[
            b"\x1b[31mred\x1b[0m \x1b[1;34mbold\x1b[0m \x1b[4:3mcurly\x1b[0m\x1b[58;2;9;8;7m",
            b"\r\n\x1b[44m  \x1b[0m\x1b[38;2;10;20;30mtruecolor\x1b[0m\x1b[2;4;5;8;9;53mattrs",
            b"\x1b[2J\x1b[H\x1b[?5hinverse mode\x1b[?5l",
            b"\x1b[2J\x1b[H0123456789A\xe7\x95\x8cB",
            b"\x1b[2J\x1b[He\xcc\x81 \xf0\x9f\x91\xa9\xe2\x80\x8d\xf0\x9f\x92\xbb end",
            b"\x1b]10;rgb:ff/80/00\x1b\\\x1b]11;rgb:00/40/ff\x1b\\\x1b[2J\x1b[Hdefaults",
            b"\x1b[2J\x1b[H\x1b[7mone\x1b[0m two\r\n\x1b[41mthree\x1b[0m",
        ];
        let mut sources: Vec<_> = (0..2).map(|_| terminal(COLS, ROWS)).collect();
        let mut renderers = [Renderer::new().unwrap(), Renderer::new().unwrap()];
        let mut hosts: Vec<_> = (0..2)
            .map(|_| {
                let mut host = terminal(COLS, ROWS);
                host.vt_write(b"\x1b[?7l");
                host
            })
            .collect();
        let mut frames = [Vec::new(), Vec::new()];
        for (index, step) in steps.iter().enumerate() {
            for source in &mut sources {
                source.vt_write(step);
            }
            if index == 1 {
                for source in &sources {
                    let start = source
                        .grid_ref(Point::Active(PointCoordinate { x: 1, y: 0 }))
                        .unwrap();
                    let end = source
                        .grid_ref(Point::Active(PointCoordinate { x: 4, y: 1 }))
                        .unwrap();
                    let selection = Selection::new(start, end, false);
                    source.set_selection(Some(&selection)).unwrap();
                }
            }
            if index == 4 {
                for source in &sources {
                    source.set_selection(None).unwrap();
                }
            }
            for which in 0..2 {
                frames[which].clear();
                if which == 0 {
                    renderers[which]
                        .draw_at(&sources[which], &mut frames[which], 0, false)
                        .unwrap();
                } else {
                    renderers[which]
                        .draw_reference_at(&sources[which], &mut frames[which], 0, false)
                        .unwrap();
                }
                hosts[which].vt_write(&frames[which]);
            }
            assert_eq!(
                grid_snapshot(&hosts[0], COLS, ROWS),
                grid_snapshot(&hosts[1], COLS, ROWS),
                "step {index}"
            );
        }
        // A forced redraw at an offset must stay equivalent as well.
        for which in 0..2 {
            frames[which].clear();
            if which == 0 {
                renderers[which]
                    .draw_at(&sources[which], &mut frames[which], 1, true)
                    .unwrap();
            } else {
                renderers[which]
                    .draw_reference_at(&sources[which], &mut frames[which], 1, true)
                    .unwrap();
            }
            hosts[which].vt_write(&frames[which]);
        }
        assert_eq!(
            grid_snapshot(&hosts[0], COLS, ROWS),
            grid_snapshot(&hosts[1], COLS, ROWS),
            "forced redraw"
        );
        // A clean frame produces nothing from either implementation.
        for which in 0..2 {
            frames[which].clear();
            if which == 0 {
                renderers[which]
                    .draw_at(&sources[which], &mut frames[which], 0, false)
                    .unwrap();
            } else {
                renderers[which]
                    .draw_reference_at(&sources[which], &mut frames[which], 0, false)
                    .unwrap();
            }
            assert!(frames[which].is_empty(), "clean frame {which}");
        }
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
