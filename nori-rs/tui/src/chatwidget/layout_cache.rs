//! A history cell shown live in the viewport, with its layout cached per width.
//!
//! The viewport is drawn at animation rate while a turn runs, and each frame asks the active cell
//! for its height and then for its rows, from several layout passes. Laying the cell out once per
//! width, and again only after the cell changed, keeps a frame proportional to the rows it shows
//! rather than to everything the cell contains.

use std::cell::RefCell;
use std::sync::Arc;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::text::Text;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use crate::history_cell::HistoryCell;
use crate::render::renderable::Renderable;

struct Layout {
    width: u16,
    lines: Arc<Vec<Line<'static>>>,
    height: u16,
}

/// An optional cell plus the layout last computed for it.
#[derive(Default)]
pub(super) struct CachedCell {
    cell: Option<Box<dyn HistoryCell>>,
    layout: RefCell<Option<Layout>>,
    /// Number of layouts computed, for tests that count work.
    #[cfg(test)]
    layouts: std::cell::Cell<usize>,
}

impl CachedCell {
    pub(super) fn is_some(&self) -> bool {
        self.cell.is_some()
    }

    pub(super) fn as_ref(&self) -> Option<&dyn HistoryCell> {
        self.cell.as_deref()
    }

    /// Mutable access to the cell. The cached layout is dropped because the cell may change.
    pub(super) fn as_mut(&mut self) -> Option<&mut Box<dyn HistoryCell>> {
        self.layout.get_mut().take();
        self.cell.as_mut()
    }

    pub(super) fn take(&mut self) -> Option<Box<dyn HistoryCell>> {
        self.layout.get_mut().take();
        self.cell.take()
    }

    pub(super) fn set(&mut self, cell: Option<Box<dyn HistoryCell>>) {
        self.layout.get_mut().take();
        self.cell = cell;
    }

    /// Remove the cell when `predicate` holds for it.
    pub(super) fn take_if(
        &mut self,
        predicate: impl FnOnce(&mut Box<dyn HistoryCell>) -> bool,
    ) -> Option<Box<dyn HistoryCell>> {
        self.layout.get_mut().take();
        self.cell.take_if(predicate)
    }

    #[cfg(test)]
    pub(super) fn layout_count(&self) -> usize {
        self.layouts.get()
    }

    fn with_layout<T>(&self, width: u16, read: impl FnOnce(&Layout) -> T) -> Option<T> {
        let cell = self.cell.as_ref()?;
        let mut layout = self.layout.borrow_mut();
        if layout
            .as_ref()
            .is_none_or(|layout| layout.width != width || cell.animated())
        {
            #[cfg(test)]
            self.layouts.set(self.layouts.get() + 1);
            *layout = Some(Layout {
                width,
                lines: Arc::new(cell.display_lines(width)),
                height: HistoryCell::desired_height(cell.as_ref(), width),
            });
        }
        layout.as_ref().map(read)
    }
}

impl Renderable for CachedCell {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let Some(lines) = self.with_layout(area.width, |layout| Arc::clone(&layout.lines)) else {
            return;
        };
        // When the cell is taller than its area the newest rows are the ones that matter.
        let overflow = lines.len().saturating_sub(usize::from(area.height));
        Paragraph::new(Text::from(lines[overflow..].to_vec())).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.with_layout(width, |layout| layout.height).unwrap_or(0)
    }
}

/// A [`CachedCell`] capped so that the viewport never covers the rows history is flushed into.
///
/// History lines are inserted by scrolling the rows above the viewport, which needs two free rows
/// there. The cap leaves those two rows, the composer area, and one row of margin.
pub(super) struct Capped<'a> {
    pub(super) cell: &'a CachedCell,
    pub(super) bottom_pane: &'a crate::bottom_pane::BottomPane,
    /// Terminal rows, or zero when unknown (no cap).
    pub(super) screen_rows: u16,
}

impl Renderable for Capped<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.cell.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let height = self.cell.desired_height(width);
        if self.screen_rows == 0 {
            return height;
        }
        let composer_rows = self.bottom_pane.desired_height(width).saturating_add(1);
        height.min(
            self.screen_rows
                .saturating_sub(composer_rows.saturating_add(3)),
        )
    }
}

pub(super) struct PositionedBottomPane<'a> {
    pub(super) pane: &'a crate::bottom_pane::BottomPane,
    pub(super) area: &'a std::cell::Cell<Option<Rect>>,
}

impl Renderable for PositionedBottomPane<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.area.set(Some(area));
        self.pane.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.pane.desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.pane.cursor_pos(area)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// However many layout passes a frame makes, a cell is laid out once per width, and again only
    /// after it changes.
    #[test]
    fn a_cell_is_laid_out_once_per_width_and_change() {
        let mut cached = CachedCell::default();
        cached.set(Some(Box::new(crate::history_cell::new_user_prompt(
            "a prompt long enough to wrap at a narrow width".repeat(4),
        ))));
        let area = Rect::new(0, 0, 80, 10);
        let mut buf = Buffer::empty(area);
        for _ in 0..3 {
            let _ = cached.desired_height(80);
            cached.render(area, &mut buf);
        }
        assert_eq!(cached.layout_count(), 1);
        let _ = cached.desired_height(40);
        assert_eq!(cached.layout_count(), 2);
        let _ = cached.as_mut();
        let _ = cached.desired_height(40);
        assert_eq!(cached.layout_count(), 3);
    }
}
