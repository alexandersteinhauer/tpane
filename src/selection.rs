use anyhow::Result;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use libghostty_vt::{
    Terminal,
    screen::TrackedGridRef,
    selection::{FormatOptions, Selection},
    terminal::{Point, PointCoordinate},
};

#[derive(Default)]
pub struct SelectionState {
    anchor: Option<TrackedGridRef>,
    dragging: bool,
}

impl SelectionState {
    pub fn cancel_drag(&mut self) {
        self.dragging = false;
    }

    pub fn clear(&mut self, terminal: &Terminal<'_, '_>) -> Result<()> {
        self.dragging = false;
        terminal.set_selection(None)?;
        Ok(())
    }

    pub fn mouse(
        &mut self,
        terminal: &mut Terminal<'_, '_>,
        event: MouseEvent,
    ) -> Result<Option<Vec<u8>>> {
        let point = Point::Viewport(PointCoordinate {
            x: event.column.min(terminal.cols()?.saturating_sub(1)),
            y: u32::from(event.row.min(terminal.rows()?.saturating_sub(1))),
        });
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.clear(terminal)?;
                if let Some(anchor) = &mut self.anchor {
                    anchor.set(terminal, point)?;
                } else {
                    self.anchor = Some(terminal.track_grid_ref(point)?);
                }
                self.dragging = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.dragging => {
                if let Some(anchor) = &self.anchor
                    && let Some(start) = anchor.snapshot(terminal)?
                {
                    let end = terminal.grid_ref(point)?;
                    terminal.set_selection(Some(&Selection::new(start, end, false)))?;
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.dragging => {
                self.dragging = false;
                return selected_text(terminal);
            }
            _ => {}
        }
        Ok(None)
    }
}

fn selected_text(terminal: &Terminal<'_, '_>) -> Result<Option<Vec<u8>>> {
    Ok(terminal
        .format_selection_alloc(None, FormatOptions::new().with_unwrap(true).with_trim(true))?
        .map(|bytes| bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use libghostty_vt::{TerminalOptions, terminal::ScrollViewport};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn selects_and_copies_from_independent_scrollback() {
        let mut terminal = Terminal::new(TerminalOptions {
            cols: 20,
            rows: 2,
            max_scrollback: 100,
        })
        .unwrap();
        terminal.vt_write(b"old line\r\nsecond\r\nthird\r\n");
        terminal.scroll_viewport(ScrollViewport::Top);
        let mut selection = SelectionState::default();
        selection
            .mouse(
                &mut terminal,
                mouse(MouseEventKind::Down(MouseButton::Left), 0, 0),
            )
            .unwrap();
        selection
            .mouse(
                &mut terminal,
                mouse(MouseEventKind::Drag(MouseButton::Left), 7, 0),
            )
            .unwrap();
        let copied = selection
            .mouse(
                &mut terminal,
                mouse(MouseEventKind::Up(MouseButton::Left), 7, 0),
            )
            .unwrap();
        assert_eq!(copied.unwrap(), b"old line");
        selection.clear(&terminal).unwrap();
        assert!(selected_text(&terminal).unwrap().is_none());
    }
}
