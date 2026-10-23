//! Frames: the screen as rows of styled runs, for viewers that draw cells
//! themselves (e.g. a browser) instead of interpreting VT output.
//!
//! A full frame (`full: true`) carries every row. An incremental frame
//! carries only rows that changed since the previous frame (`seq - 1`); rows
//! not listed are unchanged. Every frame carries the complete cursor, title,
//! modes, default colours and scrollbar. [`ScreenModel`] is a reference
//! viewer: it rebuilds the screen from a full frame plus the incremental
//! frames after it.
//!
//! Each listed row is complete: its runs cover the row from column 0, left
//! to right; cells after the last run are blank with the default colours
//! (`foreground`/`background`, or the viewer's own when `None`). Trailing
//! blank cells that look like default blanks are omitted.
//!
//! A run's text is either a sequence of single-codepoint cells, each
//! `cells / chars` columns wide (1, or 2 for wide characters), or, when
//! `cluster` is set, one grapheme cluster occupying `cells` columns. Viewers
//! therefore never need their own width or grapheme tables.
//!
//! Colours are resolved to RGB (palette indices are looked up) or `None` for
//! the default. `inverse` is already combined with the terminal's reverse
//! video mode and the selection; viewers swap foreground and background.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[cfg(feature = "serde")]
use crate::event::is_false;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Frame {
    /// Screen version. An incremental frame `n` applies on top of the state
    /// of frame `n - 1`; a full frame `n` is the complete state at version `n`.
    pub seq: u64,
    pub full: bool,
    pub cols: u16,
    pub rows: u16,
    /// Changed rows (all rows in a full frame), in ascending order.
    pub lines: Vec<Line>,
    /// Styles referenced by runs, by index; local to this frame.
    pub styles: Vec<CellStyle>,
    pub cursor: Cursor,
    pub title: String,
    pub modes: Modes,
    /// The terminal's default colours when a program set them (OSC 10/11).
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub foreground: Option<Rgb>,
    #[cfg_attr(
        feature = "serde",
        serde(default, skip_serializing_if = "Option::is_none")
    )]
    pub background: Option<Rgb>,
    pub scrollbar: Scrollbar,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Line {
    pub y: u16,
    pub runs: Vec<Run>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Run {
    pub text: String,
    /// Index into [`Frame::styles`].
    pub style: u16,
    /// Columns covered.
    pub cells: u16,
    #[cfg_attr(feature = "serde", serde(default, skip_serializing_if = "is_false"))]
    pub cluster: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Rgb(pub u8, pub u8, pub u8);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct CellStyle {
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub fg: Option<Rgb>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub bg: Option<Rgb>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub bold: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub italic: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub faint: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_none"))]
    pub underline: Underline,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "Option::is_none"))]
    pub underline_color: Option<Rgb>,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub strikethrough: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub overline: bool,
    /// Swap foreground and background when drawing.
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub inverse: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub blink: bool,
    #[cfg_attr(feature = "serde", serde(skip_serializing_if = "is_false"))]
    pub invisible: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Underline {
    #[default]
    None,
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

#[cfg(feature = "serde")]
fn is_none(underline: &Underline) -> bool {
    *underline == Underline::None
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Cursor {
    /// Viewport position; meaningless when not visible.
    pub x: u16,
    pub y: u16,
    /// False when hidden by the program or outside the viewport.
    pub visible: bool,
    pub shape: CursorShape,
    pub blinking: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum CursorShape {
    #[default]
    Block,
    BlockHollow,
    Bar,
    Underline,
}

/// Terminal modes a viewer needs to route input and pick its own behaviour.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Modes {
    /// Which mouse events the program wants; when `Off`, mouse input selects
    /// text and the wheel scrolls history instead.
    pub mouse: MouseTracking,
    pub bracketed_paste: bool,
    pub alternate_screen: bool,
    pub focus_reporting: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MouseTracking {
    #[default]
    Off,
    /// Presses only (mode 9).
    X10,
    /// Presses and releases (1000).
    Normal,
    /// Also motion while a button is held (1002).
    Button,
    /// All motion (1003).
    Any,
}

/// Rows of history and where the viewport is: `offset` of `total` rows,
/// showing `len` rows. `offset + len == total` when following the output.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Scrollbar {
    pub total: u64,
    pub offset: u64,
    pub len: u64,
}

/// One cell of a [`ScreenModel`]. The second column of a wide character is
/// a spacer with empty text and width 0.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelCell {
    pub text: String,
    pub style: CellStyle,
    pub width: u8,
}

/// A reference viewer: applies frames and exposes the resulting grid.
#[derive(Clone, Debug, Default)]
pub struct ScreenModel {
    pub seq: Option<u64>,
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Vec<ModelCell>>,
    pub cursor: Cursor,
    pub title: String,
    pub modes: Modes,
}

impl ScreenModel {
    /// Applies a frame. Incremental frames must follow the previous frame
    /// directly; a full frame can always be applied.
    pub fn apply(&mut self, frame: &Frame) -> Result<(), String> {
        if frame.full {
            self.cols = frame.cols;
            self.rows = frame.rows;
            self.cells = vec![Self::blank_row(frame.cols); usize::from(frame.rows)];
        } else {
            match self.seq {
                Some(seq) if seq + 1 == frame.seq => {}
                seq => {
                    return Err(format!(
                        "incremental frame {} does not follow {seq:?}",
                        frame.seq
                    ));
                }
            }
            if (frame.cols, frame.rows) != (self.cols, self.rows) {
                return Err("incremental frame changed the size".into());
            }
        }
        for line in &frame.lines {
            let row = self
                .cells
                .get_mut(usize::from(line.y))
                .ok_or("row out of range")?;
            *row = Self::blank_row(frame.cols);
            let mut x = 0usize;
            for run in &line.runs {
                let style = *frame
                    .styles
                    .get(usize::from(run.style))
                    .ok_or("style out of range")?;
                let mut place = |text: String, width: usize| -> Result<(), String> {
                    let cell = row.get_mut(x).ok_or("run exceeds the row")?;
                    *cell = ModelCell {
                        text,
                        style,
                        width: width as u8,
                    };
                    for spacer in 1..width {
                        *row.get_mut(x + spacer).ok_or("run exceeds the row")? = ModelCell {
                            text: String::new(),
                            style,
                            width: 0,
                        };
                    }
                    x += width;
                    Ok(())
                };
                if run.cluster {
                    place(run.text.clone(), usize::from(run.cells))?;
                } else {
                    let chars = run.text.chars().count();
                    if chars == 0 || usize::from(run.cells) % chars != 0 {
                        return Err("run cells do not match its text".into());
                    }
                    let width = usize::from(run.cells) / chars;
                    for ch in run.text.chars() {
                        place(ch.to_string(), width)?;
                    }
                }
            }
        }
        self.seq = Some(frame.seq);
        self.cursor = frame.cursor;
        self.title.clone_from(&frame.title);
        self.modes = frame.modes;
        Ok(())
    }

    /// The grid as text: one line per row, trailing blanks trimmed, trailing
    /// empty lines removed.
    pub fn text(&self) -> String {
        let mut lines: Vec<String> = self
            .cells
            .iter()
            .map(|row| {
                let line: String = row.iter().map(|cell| cell.text.as_str()).collect();
                line.trim_end().to_owned()
            })
            .collect();
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines.join("\n")
    }

    fn blank_row(cols: u16) -> Vec<ModelCell> {
        vec![
            ModelCell {
                text: " ".into(),
                style: CellStyle::default(),
                width: 1,
            };
            usize::from(cols)
        ]
    }
}
