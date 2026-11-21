//! Drawing the editor into a ratatui buffer.
//!
//! The rows come from [Layout] alone; the only state drawing keeps is the first row shown, which
//! moves just enough to keep the primary cursor on screen and never leaves rows empty below the
//! text while text is scrolled off the top.

use std::ops::Range;

use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use unicode_segmentation::UnicodeSegmentation;

use crate::chips::ChipTone;
use crate::editor::Editor;
use crate::keys::CursorShape;
use crate::layout::width_of;

/// How the editor is drawn.
#[derive(Clone, Debug)]
pub struct EditorStyle {
    pub text: Style,
    pub selection: Style,
    /// Cursors drawn as blocks: secondary cursors, and the primary one outside Insert mode.
    pub cursor: Style,
    pub attachment: Style,
    pub reference: Style,
    pub directive: Style,
    pub prompt_label: Style,
}

impl Default for EditorStyle {
    fn default() -> Self {
        EditorStyle {
            text: Style::default(),
            selection: Style::default().add_modifier(Modifier::REVERSED),
            cursor: Style::default().add_modifier(Modifier::REVERSED),
            attachment: Style::default().add_modifier(Modifier::BOLD),
            reference: Style::default().add_modifier(Modifier::UNDERLINED),
            directive: Style::default().add_modifier(Modifier::ITALIC),
            prompt_label: Style::default().add_modifier(Modifier::DIM),
        }
    }
}

/// A styled stretch of the text (syntax, search matches), drawn under the selections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Highlight {
    pub range: Range<usize>,
    pub style: Style,
}

impl Editor {
    /// Draw the editor into `area` and return where the terminal cursor goes.
    pub fn render(
        &self,
        area: Rect,
        buf: &mut Buffer,
        style: &EditorStyle,
        highlights: &[Highlight],
    ) -> Option<Position> {
        if area.is_empty() {
            return None;
        }
        let text = &self.text.text;
        let View {
            layout,
            visible,
            scroll,
            head_row,
            head_col,
        } = self.view(area);

        let selections: Vec<Range<usize>> = self
            .selections()
            .iter()
            .filter(|selection| selection.selected)
            .map(|selection| selection.range(text))
            .collect();
        let block_primary = self.cursor_shape() != CursorShape::Bar;
        let heads: Vec<usize> = self
            .selections()
            .iter()
            .skip(usize::from(!block_primary))
            .map(|selection| selection.head)
            .collect();

        for (offset, row) in layout.rows.iter().skip(scroll).take(visible).enumerate() {
            let y = area.y + u16::try_from(offset).unwrap_or(u16::MAX);
            let mut x = area.x;
            for (at, grapheme) in text[row.range.clone()].grapheme_indices(true) {
                if x >= area.right() {
                    break;
                }
                let at = row.range.start + at;
                let mut cell = style.text;
                if let Some(highlight) = highlights.iter().find(|h| h.range.contains(&at)) {
                    cell = cell.patch(highlight.style);
                }
                let chip = single_char(grapheme).and_then(|ch| self.chips.of_char(ch));
                if let Some(chip) = chip {
                    cell = cell.patch(match chip.tone {
                        ChipTone::Attachment => style.attachment,
                        ChipTone::Reference => style.reference,
                        ChipTone::Directive => style.directive,
                    });
                }
                if selections.iter().any(|range| range.contains(&at)) {
                    cell = cell.patch(style.selection);
                }
                if heads.contains(&at) {
                    cell = cell.patch(style.cursor);
                }
                match chip {
                    Some(chip) => {
                        let room = usize::from(area.right() - x);
                        buf.set_stringn(x, y, &chip.label, room, cell);
                        x = x.saturating_add(u16::try_from(chip.width()).unwrap_or(u16::MAX));
                    }
                    None if grapheme == "\t" => {
                        let room = usize::from(area.right() - x);
                        buf.set_stringn(x, y, "    ", room, cell);
                        x = x.saturating_add(4);
                    }
                    None if grapheme.chars().all(char::is_whitespace) => {
                        buf[(x, y)].set_symbol(" ").set_style(cell);
                        x = x.saturating_add(1);
                    }
                    None => {
                        let width = width_of(grapheme, &self.chips);
                        if width == 0 {
                            continue;
                        }
                        buf[(x, y)].set_symbol(grapheme).set_style(cell);
                        x = x.saturating_add(u16::try_from(width).unwrap_or(1));
                    }
                }
            }
            // A cursor after the last character of a line is drawn on the blank cell there.
            if x < area.right() && heads.contains(&row.range.end) && row.line_end {
                buf[(x, y)].set_style(style.text.patch(style.cursor));
            }
        }

        if let Some((label, line, cursor)) = self.prompt_line() {
            let y = area.bottom() - 1;
            let used = buf.set_stringn(
                area.x,
                y,
                label,
                usize::from(area.width),
                style.prompt_label,
            );
            let start = used.0.saturating_add(1);
            if start < area.right() {
                buf.set_stringn(
                    start,
                    y,
                    line,
                    usize::from(area.right() - start),
                    style.text,
                );
            }
            let column = crate::layout::columns(&line[..cursor], &self.chips);
            let x = start.saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
            return (x < area.right()).then_some(Position::new(x, y));
        }

        let row = head_row.checked_sub(scroll).filter(|row| *row < visible)?;
        let x = area.x
            + u16::try_from(head_col)
                .unwrap_or(u16::MAX)
                .min(area.width - 1);
        let y = area.y + u16::try_from(row).unwrap_or(u16::MAX);
        Some(Position::new(x, y))
    }
}

/// Where things are when the editor is drawn in an area.
struct View {
    layout: std::rc::Rc<crate::layout::Layout>,
    /// Text rows that fit.
    visible: usize,
    /// First row shown.
    scroll: usize,
    head_row: usize,
    head_col: usize,
}

impl Editor {
    fn view(&self, area: Rect) -> View {
        let layout = self.layout(area.width);
        let prompt_rows = u16::from(self.prompt.is_some());
        let visible = usize::from(area.height.saturating_sub(prompt_rows)).max(1);
        let (head_row, head_col) = layout.position(&self.text.text, &self.chips, self.cursor());
        let scroll = settle_scroll(self.scroll.get(), head_row, visible, layout.rows.len());
        self.scroll.set(scroll);
        View {
            layout,
            visible,
            scroll,
            head_row,
            head_col,
        }
    }

    /// Where the terminal cursor goes when the editor is drawn in `area` (what [Editor::render]
    /// returns), without drawing.
    pub fn cursor_position(&self, area: Rect) -> Option<Position> {
        if area.is_empty() {
            return None;
        }
        if let Some((label, line, cursor)) = self.prompt_line() {
            let start =
                area.x + u16::try_from(crate::layout::columns(label, &self.chips) + 1).unwrap_or(0);
            let column = crate::layout::columns(&line[..cursor], &self.chips);
            let x = start.saturating_add(u16::try_from(column).unwrap_or(u16::MAX));
            return (x < area.right()).then_some(Position::new(x, area.bottom() - 1));
        }
        let view = self.view(area);
        let row = view
            .head_row
            .checked_sub(view.scroll)
            .filter(|row| *row < view.visible)?;
        let x = area.x
            + u16::try_from(view.head_col)
                .unwrap_or(u16::MAX)
                .min(area.width - 1);
        let y = area.y + u16::try_from(row).unwrap_or(u16::MAX);
        Some(Position::new(x, y))
    }

    /// Put the primary cursor at screen position `(column, row)` of `area`, as a click does.
    /// Returns whether the position is on the text.
    pub fn click(&mut self, area: Rect, column: u16, row: u16) -> bool {
        if !area.contains(Position::new(column, row)) {
            return false;
        }
        let view = self.view(area);
        let row = usize::from(row - area.y) + view.scroll;
        if row >= view.layout.rows.len() {
            return false;
        }
        let at = view.layout.offset_at(
            &self.text.text,
            &self.chips,
            row,
            usize::from(column - area.x),
        );
        self.set_cursor(at);
        true
    }
}

/// The first row to show: unchanged unless the cursor row would be off screen, and never so far
/// down that rows go empty under the text while text is hidden above.
fn settle_scroll(scroll: usize, cursor_row: usize, visible: usize, rows: usize) -> usize {
    let mut scroll = scroll.min(rows.saturating_sub(visible));
    if cursor_row < scroll {
        scroll = cursor_row;
    } else if cursor_row >= scroll + visible {
        scroll = cursor_row + 1 - visible;
    }
    scroll
}

fn single_char(grapheme: &str) -> Option<char> {
    let mut chars = grapheme.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => Some(ch),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn scroll_moves_only_as_far_as_the_cursor_needs() {
        assert_eq!(settle_scroll(0, 2, 3, 10), 0);
        assert_eq!(settle_scroll(0, 5, 3, 10), 3);
        assert_eq!(settle_scroll(4, 1, 3, 10), 1);
        // Text shrank: no empty rows under it while rows are hidden above.
        assert_eq!(settle_scroll(6, 3, 3, 4), 1);
    }
}
