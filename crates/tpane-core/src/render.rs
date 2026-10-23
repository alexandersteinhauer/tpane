//! The renderer: one dirty-row walk over Ghostty's render state, with its
//! output going to a sink.
//!
//! The walk owns everything that decides *what* changed and *how* a cell
//! looks: row and cursor dirtiness, the style-id cache, wide cells and their
//! spacer tails, graphemes, erased-cell backgrounds, reverse video and the
//! selection. Sinks only encode: [`Renderer::draw_at`] produces VT output for
//! a host terminal, [`Renderer::frame`] produces a [`Frame`] of styled runs.

use std::io::Write;

use anyhow::Result;
use libghostty_vt::{
    RenderState, Terminal,
    error::Error,
    render::{CellIterator, CursorVisualStyle, Dirty, RowIterator},
    screen::{CellWide, Screen},
    style::{Id as StyleId, RgbColor, Style, StyleColor, Underline},
    terminal::Mode,
};

use crate::frame::{
    CellStyle, Cursor, CursorShape, Frame, Line, Modes, MouseTracking, Rgb, Run, Scrollbar,
    Underline as FrameUnderline,
};

/// Cell content as the walk sees it.
enum Text<'a> {
    /// No text, or the blank column left by a wide character that wrapped.
    Blank,
    Char(char),
    /// The cell's grapheme cluster (possibly a single codepoint).
    Str(&'a str),
}

#[derive(Clone, Copy)]
struct CursorInfo {
    /// Viewport position; `None` outside the viewport.
    position: Option<(u16, u16)>,
    visible: bool,
    style: CursorVisualStyle,
    blinking: bool,
}

impl CursorInfo {
    /// DECSCUSR shape.
    fn shape(self) -> u8 {
        let base = match self.style {
            CursorVisualStyle::Underline => 3,
            CursorVisualStyle::Bar => 5,
            _ => 1,
        };
        base + u8::from(!self.blinking)
    }
}

/// Receives the walk's output. Rows arrive in ascending order; every cell of
/// a dirty row arrives, except wide-character spacer tails.
trait Sink {
    /// `base` is the style of unstyled cells (terminal default colours).
    fn begin(&mut self, full: bool, base: &Style, palette: &[RgbColor; 256]) -> Result<()>;
    fn row(&mut self, y: u16) -> Result<()>;
    fn cell(
        &mut self,
        style: Style,
        text: Text<'_>,
        wide: bool,
        palette: &[RgbColor; 256],
    ) -> Result<()>;
    fn end_row(&mut self) -> Result<()>;
    fn finish(&mut self, cursor: CursorInfo) -> Result<()>;
}

struct Walker {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    // Reused grapheme scratch space: cleared per cell, allocated once.
    text: String,
    // Cursor of the last walk, as (x, absolute VT row, shape), to detect
    // cursor-only changes.
    cursor: Option<(u16, u32, u8)>,
}

pub struct Renderer {
    walker: Walker,
    buffer: Vec<u8>,
}

impl Renderer {
    pub fn new() -> Result<Self> {
        Ok(Self {
            walker: Walker {
                state: RenderState::new()?,
                rows: RowIterator::new()?,
                cells: CellIterator::new()?,
                text: String::new(),
                cursor: None,
            },
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

    /// Writes VT output that brings a host terminal, showing this terminal
    /// from row `top`, up to date: dirty rows (all rows if `force`), then the
    /// cursor. The output opens a synchronized update and leaves it open;
    /// the caller commits it after composing anything on top. Writes
    /// nothing when nothing changed.
    pub fn draw_at(
        &mut self,
        terminal: &Terminal<'static, '_>,
        output: &mut impl Write,
        top: u16,
        force: bool,
    ) -> Result<()> {
        let mut sink = VtSink {
            buffer: &mut self.buffer,
            top,
            previous: None,
        };
        if self.walker.walk(terminal, &mut sink, top, force, false)? {
            // App commits synchronized output only after composing its overlay.
            output.write_all(&self.buffer)?;
            output.flush()?;
        }
        Ok(())
    }

    /// Renders the changes since the last render as a [`Frame`], or all rows
    /// if `full`. Returns `None` when nothing changed, unless `always`
    /// (e.g. the title or modes changed), which yields a frame without rows.
    /// The caller assigns `seq`.
    pub fn frame(
        &mut self,
        terminal: &Terminal<'static, '_>,
        full: bool,
        always: bool,
    ) -> Result<Option<Frame>> {
        let meta = FrameMeta::read(terminal)?;
        self.frame_with(terminal, meta, full, always)
    }

    /// [`Renderer::frame`] with metadata the caller already read.
    pub fn frame_with(
        &mut self,
        terminal: &Terminal<'static, '_>,
        meta: FrameMeta,
        full: bool,
        always: bool,
    ) -> Result<Option<Frame>> {
        let mut frame = Frame::default();
        let mut sink = FrameSink::new(&mut frame);
        if !self.walker.walk(terminal, &mut sink, 0, full, always)? {
            return Ok(None);
        }
        meta.apply(&mut frame);
        Ok(Some(frame))
    }
}

/// Everything in a frame besides rows and cursor. Changes here do not mark
/// rows dirty, so frame producers compare it to decide on `always`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FrameMeta {
    pub cols: u16,
    pub rows: u16,
    pub title: String,
    pub modes: Modes,
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub scrollbar: Scrollbar,
}

impl FrameMeta {
    pub fn read(terminal: &Terminal<'_, '_>) -> Result<Self> {
        let mouse = if terminal.mode(Mode::ANY_MOUSE)? {
            MouseTracking::Any
        } else if terminal.mode(Mode::BUTTON_MOUSE)? {
            MouseTracking::Button
        } else if terminal.mode(Mode::NORMAL_MOUSE)? {
            MouseTracking::Normal
        } else if terminal.mode(Mode::X10_MOUSE)? {
            MouseTracking::X10
        } else {
            MouseTracking::Off
        };
        let scrollbar = terminal.scrollbar()?;
        Ok(Self {
            cols: terminal.cols()?,
            rows: terminal.rows()?,
            title: terminal.title()?.to_owned(),
            modes: Modes {
                mouse,
                bracketed_paste: terminal.mode(Mode::BRACKETED_PASTE)?,
                alternate_screen: terminal.active_screen()? == Screen::Alternate,
                focus_reporting: terminal.mode(Mode::FOCUS_EVENT)?,
            },
            foreground: terminal.fg_color()?.map(rgb),
            background: terminal.bg_color()?.map(rgb),
            scrollbar: Scrollbar {
                total: scrollbar.total,
                offset: scrollbar.offset,
                len: scrollbar.len,
            },
        })
    }

    fn apply(self, frame: &mut Frame) {
        frame.cols = self.cols;
        frame.rows = self.rows;
        frame.title = self.title;
        frame.modes = self.modes;
        frame.foreground = self.foreground;
        frame.background = self.background;
        frame.scrollbar = self.scrollbar;
    }
}

impl Walker {
    /// Walks dirty rows (all rows if `force`) into `sink`. Returns false,
    /// without calling the sink, when neither rows nor cursor changed and
    /// `always` is false. `top` only distinguishes cursor positions for
    /// change detection, as VT output places the cursor at `top + y`.
    fn walk(
        &mut self,
        terminal: &Terminal<'static, '_>,
        sink: &mut impl Sink,
        top: u16,
        force: bool,
        always: bool,
    ) -> Result<bool> {
        let snapshot = self.state.update(terminal)?;
        let dirty = if force {
            Dirty::Full
        } else {
            snapshot.dirty()?
        };
        // Ghostty's cell dirty flags do not include cursor-only changes.
        // TUIs often advance over blank prompt cells instead of writing spaces.
        let visible = snapshot.cursor_visible()?;
        let cursor_info = CursorInfo {
            position: snapshot.cursor_viewport()?.map(|p| (p.x, p.y)),
            visible,
            style: snapshot.cursor_visual_style()?,
            blinking: snapshot.cursor_blinking()?,
        };
        let cursor = match cursor_info.position {
            Some((x, y)) if visible => {
                Some((x, u32::from(top) + u32::from(y), cursor_info.shape()))
            }
            _ => None,
        };
        if dirty == Dirty::Clean && cursor == self.cursor && !always {
            return Ok(false);
        }
        let colors = snapshot.colors()?;
        let default_fg = terminal.fg_color()?;
        let default_bg = terminal.bg_color()?;
        let reverse = terminal.mode(Mode::REVERSE_COLORS)?;
        // Cells without explicit styling share one base style: the terminal
        // defaults with no attributes. Explicit fg/bg colors are only read when
        // a cell is styled or carries a content background, because sinks
        // resolve palette indices themselves.
        let mut base = Style::default();
        if let Some(color) = default_fg {
            base.fg_color = StyleColor::Rgb(color);
        }
        if let Some(color) = default_bg {
            base.bg_color = StyleColor::Rgb(color);
        }
        sink.begin(dirty == Dirty::Full, &base, &colors.palette)?;
        let mut rows = self.rows.update(&snapshot)?;
        let mut y: u16 = 0;
        let mut cached_style_id: Option<StyleId> = None;
        let mut cached_style = Style::default();
        while let Some(row) = rows.next() {
            if dirty == Dirty::Full || row.dirty()? {
                sink.row(y)?;
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
                    let text = if !has_text || wide == CellWide::SpacerHead {
                        Text::Blank
                    } else if let Some(codepoint) = codepoint {
                        Text::Char(char::from_u32(codepoint).ok_or(Error::InvalidValue)?)
                    } else {
                        Text::Str(&self.text)
                    };
                    let is_wide = wide == CellWide::Wide;
                    sink.cell(style, text, is_wide, &colors.palette)?;
                    if is_wide {
                        skip_tail = true;
                    }
                    x += 1;
                }
                sink.end_row()?;
                row.set_dirty(false)?;
            }
            y += 1;
        }
        sink.finish(cursor_info)?;
        self.cursor = cursor;
        snapshot.set_dirty(Dirty::Clean)?;
        Ok(true)
    }
}

/// VT output for a host terminal.
struct VtSink<'a> {
    buffer: &'a mut Vec<u8>,
    top: u16,
    // The pen survives the absolute row-positioning writes, so a style is
    // only emitted when it changes, and it can stay across rows. Every
    // frame ends with a reset, so each call starts from an unknown pen.
    previous: Option<Style>,
}

impl Sink for VtSink<'_> {
    fn begin(&mut self, _full: bool, _base: &Style, _palette: &[RgbColor; 256]) -> Result<()> {
        self.buffer.clear();
        self.buffer.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
        Ok(())
    }

    #[inline]
    fn row(&mut self, y: u16) -> Result<()> {
        write!(
            self.buffer,
            "\x1b[{};1H",
            u32::from(self.top) + u32::from(y) + 1
        )?;
        Ok(())
    }

    #[inline]
    fn cell(
        &mut self,
        style: Style,
        text: Text<'_>,
        _wide: bool,
        palette: &[RgbColor; 256],
    ) -> Result<()> {
        if self.previous != Some(style) {
            emit_style(self.buffer, style, palette)?;
            self.previous = Some(style);
        }
        match text {
            Text::Blank => self.buffer.push(b' '),
            Text::Char(character) => {
                let mut encoded = [0; 4];
                self.buffer
                    .extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
            }
            Text::Str(text) => self.buffer.extend_from_slice(text.as_bytes()),
        }
        Ok(())
    }

    #[inline]
    fn end_row(&mut self) -> Result<()> {
        Ok(())
    }

    fn finish(&mut self, cursor: CursorInfo) -> Result<()> {
        self.buffer.extend_from_slice(b"\x1b[0m");
        if cursor.visible
            && let Some((x, y)) = cursor.position
        {
            write!(
                self.buffer,
                "\x1b[{};{}H\x1b[{} q\x1b[?25h",
                u32::from(self.top) + u32::from(y) + 1,
                x + 1,
                cursor.shape()
            )?;
        }
        Ok(())
    }
}

/// Styled runs for a [`Frame`].
struct FrameSink<'a> {
    frame: &'a mut Frame,
    /// Ghostty styles already interned this frame, parallel to `frame.styles`.
    keys: Vec<Style>,
    last: Option<(Style, u16)>,
    base_bg: Option<Rgb>,
    runs: Vec<Run>,
    /// Columns per character of the last run (1 or 2).
    width: u16,
    /// Run count, last run's text length and cells just after the last cell
    /// that is not a default blank; trailing default blanks are cut there.
    cut: (usize, usize, u16),
    y: u16,
}

impl<'a> FrameSink<'a> {
    fn new(frame: &'a mut Frame) -> Self {
        Self {
            frame,
            keys: Vec::new(),
            last: None,
            base_bg: None,
            runs: Vec::new(),
            width: 1,
            cut: (0, 0, 0),
            y: 0,
        }
    }

    fn intern(&mut self, style: Style, palette: &[RgbColor; 256]) -> u16 {
        if let Some((last, index)) = self.last
            && last == style
        {
            return index;
        }
        let index = match self.keys.iter().position(|key| *key == style) {
            Some(index) => index as u16,
            None => {
                self.keys.push(style);
                self.frame.styles.push(cell_style(style, palette));
                (self.keys.len() - 1) as u16
            }
        };
        self.last = Some((style, index));
        index
    }
}

impl Sink for FrameSink<'_> {
    fn begin(&mut self, full: bool, base: &Style, palette: &[RgbColor; 256]) -> Result<()> {
        self.frame.full = full;
        self.base_bg = resolve(base.bg_color, palette);
        Ok(())
    }

    fn row(&mut self, y: u16) -> Result<()> {
        self.y = y;
        self.runs.clear();
        self.cut = (0, 0, 0);
        Ok(())
    }

    fn cell(
        &mut self,
        style: Style,
        text: Text<'_>,
        wide: bool,
        palette: &[RgbColor; 256],
    ) -> Result<()> {
        let index = self.intern(style, palette);
        let cells = if wide { 2 } else { 1 };
        let mut buffer = [0; 4];
        let (text, cluster) = match text {
            Text::Blank => (" ", false),
            Text::Char(character) => (&*character.encode_utf8(&mut buffer), false),
            Text::Str(text) => (text, text.chars().nth(1).is_some()),
        };
        match self.runs.last_mut() {
            Some(run) if !cluster && !run.cluster && run.style == index && self.width == cells => {
                run.text.push_str(text);
                run.cells += cells;
            }
            _ => {
                self.runs.push(Run {
                    text: text.to_owned(),
                    style: index,
                    cells,
                    cluster,
                });
                self.width = cells;
            }
        }
        let resolved = &self.frame.styles[usize::from(index)];
        let default_blank = text == " "
            && resolved.bg == self.base_bg
            && !resolved.inverse
            && resolved.underline == FrameUnderline::None
            && !resolved.strikethrough
            && !resolved.overline;
        if !default_blank {
            let run = self.runs.last().expect("a run was just written");
            self.cut = (self.runs.len(), run.text.len(), run.cells);
        }
        Ok(())
    }

    fn end_row(&mut self) -> Result<()> {
        let (count, len, cells) = self.cut;
        self.runs.truncate(count);
        if let Some(run) = self.runs.last_mut() {
            run.text.truncate(len);
            run.cells = cells;
        }
        self.frame.lines.push(Line {
            y: self.y,
            runs: std::mem::take(&mut self.runs),
        });
        Ok(())
    }

    fn finish(&mut self, cursor: CursorInfo) -> Result<()> {
        let (x, y) = cursor.position.unwrap_or_default();
        self.frame.cursor = Cursor {
            x,
            y,
            visible: cursor.visible && cursor.position.is_some(),
            shape: match cursor.style {
                CursorVisualStyle::Bar => CursorShape::Bar,
                CursorVisualStyle::Underline => CursorShape::Underline,
                CursorVisualStyle::BlockHollow => CursorShape::BlockHollow,
                _ => CursorShape::Block,
            },
            blinking: cursor.blinking,
        };
        Ok(())
    }
}

fn rgb(color: RgbColor) -> Rgb {
    Rgb(color.r, color.g, color.b)
}

fn resolve(color: StyleColor, palette: &[RgbColor; 256]) -> Option<Rgb> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(index) => Some(rgb(palette[usize::from(index.0)])),
        StyleColor::Rgb(color) => Some(rgb(color)),
    }
}

fn cell_style(style: Style, palette: &[RgbColor; 256]) -> CellStyle {
    CellStyle {
        fg: resolve(style.fg_color, palette),
        bg: resolve(style.bg_color, palette),
        bold: style.bold,
        italic: style.italic,
        faint: style.faint,
        underline: match style.underline {
            Underline::None => FrameUnderline::None,
            Underline::Double => FrameUnderline::Double,
            Underline::Curly => FrameUnderline::Curly,
            Underline::Dotted => FrameUnderline::Dotted,
            Underline::Dashed => FrameUnderline::Dashed,
            _ => FrameUnderline::Single,
        },
        underline_color: resolve(style.underline_color, palette),
        strikethrough: style.strikethrough,
        overline: style.overline,
        inverse: style.inverse,
        blink: style.blink,
        invisible: style.invisible,
    }
}

impl Renderer {
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
        let snapshot = self.walker.state.update(terminal)?;
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
        let mut rows = self.walker.rows.update(&snapshot)?;
        let mut y = 0;
        let mut text = String::new();
        while let Some(row) = rows.next() {
            if dirty == Dirty::Full || row.dirty()? {
                write!(self.buffer, "\x1b[{};1H", u32::from(top) + y + 1)?;
                let mut cells = self.walker.cells.update(row)?;
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
        selection::Selection,
        terminal::{Point, PointCoordinate},
    };

    fn terminal(cols: u16, rows: u16) -> Terminal<'static, 'static> {
        Terminal::new(cols, rows).unwrap()
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
    fn renders_cursor_only_prompt_updates() {
        let mut source = terminal(20, 2);
        let mut host = terminal(20, 2);
        host.vt_write(b"\x1b[?7l");
        let mut renderer = Renderer::new().unwrap();
        source.vt_write(b"> ");
        let mut output = Vec::new();
        renderer.draw(&source, &mut output).unwrap();
        host.vt_write(&output);

        // TUIs can represent trailing spaces by moving across blank cells.
        for update in [
            b"\x1b[C".as_slice(),
            b"\x1b[C",
            b"\x1b[D",
            b"\x1b[?25l",
            b"\x1b[?25h",
            b"\x1b[6 q",
        ] {
            source.vt_write(update);
            output.clear();
            renderer.draw(&source, &mut output).unwrap();
            host.vt_write(&output);
            assert!(!output.is_empty());
            assert_eq!(host.cursor_x().unwrap(), source.cursor_x().unwrap());
            assert_eq!(
                host.mode(Mode::CURSOR_VISIBLE).unwrap(),
                source.mode(Mode::CURSOR_VISIBLE).unwrap(),
            );
        }
        output.clear();
        renderer.draw(&source, &mut output).unwrap();
        assert!(output.is_empty());
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
        // A clean frame at the same offset produces nothing from either implementation.
        for which in 0..2 {
            frames[which].clear();
            if which == 0 {
                renderers[which]
                    .draw_at(&sources[which], &mut frames[which], 1, false)
                    .unwrap();
            } else {
                renderers[which]
                    .draw_reference_at(&sources[which], &mut frames[which], 1, false)
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

    fn normalized(text: &str) -> String {
        let mut lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        while lines.last().is_some_and(|line| line.is_empty()) {
            lines.pop();
        }
        lines.join("\n")
    }

    /// Applies an incremental frame (if any) to `model` and checks the
    /// model against Ghostty's own formatter and against a fresh full frame.
    fn check_frames(
        source: &Terminal<'static, 'static>,
        renderer: &mut Renderer,
        model: &mut crate::frame::ScreenModel,
        seq: &mut u64,
        context: &str,
    ) {
        if let Some(mut frame) = renderer.frame(source, model.seq.is_none(), false).unwrap() {
            *seq += 1;
            frame.seq = *seq;
            model.apply(&frame).unwrap();
        }
        assert_eq!(
            model.text(),
            normalized(&crate::engine::screen_text(source).unwrap()),
            "{context}"
        );
        let mut fresh = crate::frame::ScreenModel::default();
        let full = Renderer::new()
            .unwrap()
            .frame(source, true, false)
            .unwrap()
            .unwrap();
        assert!(full.full);
        fresh.apply(&full).unwrap();
        assert_eq!(fresh.cells, model.cells, "{context}");
        assert_eq!(fresh.cursor, model.cursor, "{context}");
    }

    #[test]
    fn frames_rebuild_the_screen_like_ghosttys_formatter() {
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
            "\x1b[3;1H界界界界界界界".as_bytes(),
            b"\x1b[6;1Hscroll\r\nscroll\r\nmore lines here",
            b"\x1b[2;3H\x1b[K\x1b[4;5Hx\x1b[1;1H\x1b[2K",
        ];
        let mut source = terminal(COLS, ROWS);
        let mut renderer = Renderer::new().unwrap();
        let mut model = crate::frame::ScreenModel::default();
        let mut seq = 0;
        check_frames(&source, &mut renderer, &mut model, &mut seq, "empty");
        for (index, step) in steps.iter().enumerate() {
            source.vt_write(step);
            if index == 1 {
                let start = source
                    .grid_ref(Point::Active(PointCoordinate { x: 1, y: 0 }))
                    .unwrap();
                let end = source
                    .grid_ref(Point::Active(PointCoordinate { x: 4, y: 1 }))
                    .unwrap();
                source
                    .set_selection(Some(&Selection::new(start, end, false)))
                    .unwrap();
            }
            if index == 4 {
                source.set_selection(None).unwrap();
            }
            check_frames(
                &source,
                &mut renderer,
                &mut model,
                &mut seq,
                &format!("step {index}"),
            );
        }
        // Nothing changed: no frame.
        assert!(renderer.frame(&source, false, false).unwrap().is_none());
        // Cursor-only changes still produce a frame.
        source.vt_write(b"\x1b[2;2H");
        let frame = renderer.frame(&source, false, false).unwrap().unwrap();
        assert_eq!((frame.cursor.x, frame.cursor.y), (1, 1));
    }

    #[test]
    fn frame_runs_carry_resolved_styles_widths_and_clusters() {
        use crate::frame::{CursorShape, Rgb, Underline as U};
        let mut source = terminal(10, 2);
        source.vt_write(
            "\x1b[1;31mab\x1b[0m界e\u{301}\x1b[4:3;58;2;1;2;3mu\x1b[0m\x1b[44m \x1b[0m".as_bytes(),
        );
        source.vt_write(b"\x1b[5 q");
        let frame = Renderer::new()
            .unwrap()
            .frame(&source, true, false)
            .unwrap()
            .unwrap();
        assert!(frame.full);
        assert_eq!((frame.cols, frame.rows), (10, 2));
        assert_eq!(frame.lines.len(), 2);
        assert!(frame.lines[1].runs.is_empty());
        let runs = &frame.lines[0].runs;
        let summary: Vec<_> = runs
            .iter()
            .map(|run| (run.text.as_str(), run.cells, run.cluster))
            .collect();
        assert_eq!(
            summary,
            [
                ("ab", 2, false),
                ("界", 2, false),
                ("e\u{301}", 1, true),
                ("u", 1, false),
                (" ", 1, false)
            ]
        );
        let red = frame.styles[usize::from(runs[0].style)];
        assert!(red.bold);
        assert_eq!(red.fg, Some(rgb(source.color_palette().unwrap().0[1])));
        let underlined = frame.styles[usize::from(runs[3].style)];
        assert_eq!(underlined.underline, U::Curly);
        assert_eq!(underlined.underline_color, Some(Rgb(1, 2, 3)));
        // An erased blank with a background is not a default blank.
        let blue = frame.styles[usize::from(runs[4].style)];
        assert_eq!(blue.bg, Some(rgb(source.color_palette().unwrap().0[4])));
        assert_eq!(frame.cursor.shape, CursorShape::Bar);
        assert!(frame.cursor.blinking);
        assert!(frame.cursor.visible);
        assert_eq!((frame.cursor.x, frame.cursor.y), (7, 0));
    }

    #[test]
    fn frame_meta_reports_modes_title_and_colors() {
        use crate::frame::MouseTracking;
        let mut source = terminal(10, 3);
        let meta = FrameMeta::read(&source).unwrap();
        assert_eq!(meta.modes, Modes::default());
        for (sequence, mouse) in [
            (b"\x1b[?9h".as_slice(), MouseTracking::X10),
            (b"\x1b[?1000h", MouseTracking::Normal),
            (b"\x1b[?1002h", MouseTracking::Button),
            (b"\x1b[?1003h", MouseTracking::Any),
        ] {
            source.vt_write(sequence);
            assert_eq!(FrameMeta::read(&source).unwrap().modes.mouse, mouse);
        }
        source.vt_write(b"\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?9l");
        source.vt_write(
            b"\x1b[?2004h\x1b[?1004h\x1b[?1049h\x1b]2;my title\x07\x1b]11;rgb:00/40/ff\x1b\\",
        );
        let meta = FrameMeta::read(&source).unwrap();
        assert_eq!(
            meta.modes,
            Modes {
                mouse: MouseTracking::Off,
                bracketed_paste: true,
                alternate_screen: true,
                focus_reporting: true,
            }
        );
        assert_eq!(meta.title, "my title");
        assert_eq!(meta.background, Some(Rgb(0, 0x40, 0xff)));
        assert_eq!(meta.foreground, None);
        // A meta-only change yields a frame without rows when asked for.
        let mut renderer = Renderer::new().unwrap();
        renderer.frame(&source, true, false).unwrap().unwrap();
        source.vt_write(b"\x1b]2;other\x07");
        assert!(renderer.frame(&source, false, false).unwrap().is_none());
        let frame = renderer.frame(&source, false, true).unwrap().unwrap();
        assert!(frame.lines.is_empty());
        assert_eq!(frame.title, "other");
    }
}
