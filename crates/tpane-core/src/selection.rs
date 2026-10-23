use anyhow::Result;
use libghostty_vt::{
    Terminal,
    screen::TrackedGridRef,
    selection::{FormatOptions, Selection},
    terminal::{Point, PointCoordinate},
};

use crate::event::{MouseAction, MouseButton, MouseEvent};

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
        event: &MouseEvent,
    ) -> Result<Option<Vec<u8>>> {
        let point = Point::Viewport(PointCoordinate {
            x: event.x.min(terminal.cols()?.saturating_sub(1)),
            y: u32::from(event.y.min(terminal.rows()?.saturating_sub(1))),
        });
        match (event.action, event.button) {
            (MouseAction::Press, Some(MouseButton::Left)) => {
                self.clear(terminal)?;
                if let Some(anchor) = &mut self.anchor {
                    anchor.set(terminal, point)?;
                } else {
                    self.anchor = Some(terminal.track_grid_ref(point)?);
                }
                self.dragging = true;
            }
            (MouseAction::Motion, Some(MouseButton::Left)) if self.dragging => {
                if let Some(anchor) = &self.anchor
                    && let Some(start) = anchor.snapshot(terminal)?
                {
                    let end = terminal.grid_ref(point)?;
                    terminal.set_selection(Some(&Selection::new(start, end, false)))?;
                }
            }
            (MouseAction::Release, Some(MouseButton::Left)) if self.dragging => {
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
    use crate::event::Modifiers;
    use libghostty_vt::terminal::ScrollViewport;

    fn mouse(action: MouseAction, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            action,
            button: Some(MouseButton::Left),
            x,
            y,
            mods: Modifiers::NONE,
        }
    }

    #[test]
    fn selects_and_copies_from_independent_scrollback() {
        let mut terminal = Terminal::new(20, 2).unwrap();
        terminal.vt_write(b"old line\r\nsecond\r\nthird\r\n");
        terminal.scroll_viewport(ScrollViewport::Top);
        let mut selection = SelectionState::default();
        selection
            .mouse(&mut terminal, &mouse(MouseAction::Press, 0, 0))
            .unwrap();
        selection
            .mouse(&mut terminal, &mouse(MouseAction::Motion, 7, 0))
            .unwrap();
        let copied = selection
            .mouse(&mut terminal, &mouse(MouseAction::Release, 7, 0))
            .unwrap();
        assert_eq!(copied.unwrap(), b"old line");
        selection.clear(&terminal).unwrap();
        assert!(selected_text(&terminal).unwrap().is_none());
    }
}
