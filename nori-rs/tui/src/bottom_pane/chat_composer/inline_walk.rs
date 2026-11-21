//! A prompt walked back to is edited where it is in the conversation, not in the composer.
//!
//! While the walk is on a prompt of the conversation, the composer has no text box: the stage
//! draws the prompt with this editor's rows in place of its text (see `stage::focus`), the
//! cursor in them, and the composer keeps only the line that says where the walk is and what the
//! keys do, over the status line. Keys still come here, so editing is the composer's as always.

use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::ChatComposer;
use crate::bottom_pane::footer::footer_height;
use crate::bottom_pane::footer::render_footer;
use crate::stage::InlineEdit;
use crate::ui_consts::LIVE_PREFIX_COLS;

impl ChatComposer {
    /// Whether the walk is on a prompt of the conversation, which is then edited in place.
    pub(super) fn walks_inline(&self) -> bool {
        matches!(self.walk_badge(), Some(super::WalkBadge::Turn { .. }))
    }

    /// The walked prompt's text as the editor draws it at the width a prompt of the
    /// conversation wraps to in a stage `width` wide, and where the cursor is in it.
    pub(crate) fn inline_walk(&self, width: u16) -> Option<InlineEdit> {
        if !self.walks_inline() {
            return None;
        }
        // As `UserHistoryCell` wraps: its prefix, and one column free on the right.
        let wrap = width.saturating_sub(LIVE_PREFIX_COLS + 1).max(1);
        let height = self.textarea.desired_height(wrap).max(1);
        let area = Rect::new(0, 0, wrap, height);
        let mut buffer = Buffer::empty(area);
        self.textarea.render(area, &mut buffer);
        let cursor = self.textarea.cursor_pos(area);
        Some(InlineEdit {
            body: (0..height).map(|y| row_of(&buffer, y)).collect(),
            cursor,
        })
    }

    /// The rows the composer takes while a prompt is edited in place: the walk's line and the
    /// status line under it.
    pub(super) fn inline_walk_height(&self) -> u16 {
        let footer = self
            .custom_footer_height()
            .unwrap_or_else(|| footer_height(&self.footer_props()));
        1 + footer
    }

    pub(super) fn render_inline_walk(&self, area: Rect, buf: &mut Buffer) {
        let [hint, footer] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
        if let Some(badge) = self.walk_badge() {
            let indented = Rect {
                x: hint.x.saturating_add(LIVE_PREFIX_COLS),
                width: hint.width.saturating_sub(LIVE_PREFIX_COLS),
                ..hint
            };
            badge.hint(indented.width).render(indented, buf);
        }
        render_footer(footer, buf, &self.footer_props());
    }
}

/// One row of `buffer` as a line of styled text. A wide glyph's second column is not repeated.
fn row_of(buffer: &Buffer, y: u16) -> Line<'static> {
    let area = buffer.area;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut x = area.x;
    while x < area.right() {
        let cell = &buffer[(x, y)];
        let symbol = cell.symbol();
        let style = cell.style();
        match spans.last_mut() {
            Some(last) if last.style == style => last.content.to_mut().push_str(symbol),
            Some(_) | None => spans.push(Span::styled(symbol.to_owned(), style)),
        }
        let columns = u16::try_from(symbol.width().max(1)).unwrap_or(1);
        x = x.saturating_add(columns);
    }
    // Trailing blanks are the row's padding, which the prompt's frame draws itself; a blank
    // with a background or a modifier (the editor's own cursor, a selection) is kept.
    while let Some(last) = spans.last_mut() {
        let painted = last
            .style
            .bg
            .is_some_and(|bg| bg != ratatui::style::Color::Reset);
        if painted || !last.style.add_modifier.is_empty() {
            break;
        }
        let kept = last.content.trim_end().len();
        last.content.to_mut().truncate(kept);
        if !last.content.is_empty() {
            break;
        }
        spans.pop();
    }
    Line::from(spans)
}
