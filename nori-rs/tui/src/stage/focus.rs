//! Focus-backwards rewind, drawn in place.
//!
//! The focused prompt is the one row group of the transcript that changes: it becomes an amber
//! card on a raised surface with a `↶ Turn n of N` header, a rollback marker follows it, and
//! everything after it is dimmed so the blast radius of a rewind is visible before it is
//! confirmed. Editing happens in the card itself: the prompt is the editor.

use std::sync::Arc;

use nori_tui_components::Theme;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;

/// What the stage needs to draw the focused prompt and what follows it.
#[derive(Clone, Debug)]
pub(crate) struct FocusView {
    /// Index of the focused prompt in the transcript.
    pub(crate) selected: usize,
    /// Which prompt it is, and how many there are.
    pub(crate) turn: (usize, usize),
    pub(crate) edit: Option<EditView>,
    /// How many turns of file edits restoring would undo, once the journal answered.
    pub(crate) undo_turns: Option<usize>,
    /// Whether anything follows the prompt that a rewind would discard.
    pub(crate) has_downstream: bool,
}

/// The text being edited in the card and the cursor in it, as a byte offset.
#[derive(Clone, Debug)]
pub(crate) struct EditView {
    pub(crate) text: String,
    pub(crate) cursor: usize,
}

/// A whole conversation as drawn while one of its prompts is focused, for the views that show a
/// conversation that is not the stage's (a child's journal).
pub(crate) struct FocusedColumn {
    pub(crate) lines: Vec<Line<'static>>,
}

/// Every cell of `cells` as rows for `width`, the focused prompt and what follows it styled by
/// `focus`.
pub(crate) fn render_focused(
    focus: &FocusView,
    cells: &[Arc<dyn HistoryCell>],
    width: u16,
) -> FocusedColumn {
    let theme = crate::style::component_theme();
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 && !cell.is_stream_continuation() {
            lines.push(Line::default());
        }
        let plain = super::rows::hard_wrap(cell.display_lines(width), width);
        match focus.rows(index, cell, &plain, width, &theme) {
            Some(focused) => lines.extend(focused.lines),
            None => lines.extend(plain),
        }
    }
    FocusedColumn { lines }
}

/// A cell as drawn while a prompt is focused.
pub(crate) struct FocusedRows {
    pub(crate) lines: Vec<Line<'static>>,
    /// `(row, column)` inside `lines` of the caret, while the prompt is being edited.
    pub(crate) caret: Option<(usize, u16)>,
}

impl FocusView {
    /// The rows of the cell at `index`, whose ordinary rows are `plain`, or `None` when the cell is
    /// drawn as it always is (it is before the focused prompt).
    pub(crate) fn rows(
        &self,
        index: usize,
        cell: &Arc<dyn HistoryCell>,
        plain: &[Line<'static>],
        width: u16,
        theme: &Theme,
    ) -> Option<FocusedRows> {
        if index < self.selected {
            return None;
        }
        if index > self.selected {
            return Some(FocusedRows {
                lines: plain.iter().cloned().map(Stylize::dim).collect(),
                caret: None,
            });
        }
        let card = theme.rewind_surface;
        let rail = theme.accent_amber.patch(card);
        let mut lines = vec![self.header(width, theme)];
        let mut caret = None;
        if let (Some(edit), true) = (&self.edit, cell.as_any().is::<UserHistoryCell>()) {
            let (body, at) = edit_rows(edit, width, lines.len(), theme);
            lines.extend(body);
            caret = Some(at);
        } else {
            let body_width = width.saturating_sub(2).max(1);
            for line in cell.display_lines(body_width) {
                let mut spans = vec![Span::styled("▌ ", rail)];
                spans.extend(
                    line.spans
                        .into_iter()
                        .map(|span| Span::styled(span.content, span.style.patch(card))),
                );
                lines.push(pad(spans, width, card));
            }
        }
        if self.has_downstream {
            lines.push(rollback_rule(width, theme));
        }
        Some(FocusedRows { lines, caret })
    }

    fn header(&self, width: u16, theme: &Theme) -> Line<'static> {
        let card = theme.rewind_surface;
        let amber = theme.accent_amber.patch(card);
        let (turn, total) = self.turn;
        let mut spans = vec![
            Span::styled("▌ ", amber),
            Span::styled(
                format!("↶ Turn {turn} of {total}"),
                amber.add_modifier(Modifier::BOLD),
            ),
        ];
        if self.edit.is_some() {
            spans.push(Span::styled(" · editing", amber));
        }
        if let Some(turns) = self.undo_turns.filter(|turns| *turns > 0) {
            let noun = if turns == 1 { "turn" } else { "turns" };
            spans.push(Span::styled(
                format!(" · restores files from {turns} {noun}"),
                theme.text_secondary.patch(card).add_modifier(Modifier::DIM),
            ));
        }
        pad(spans, width, card)
    }
}

fn pad(mut spans: Vec<Span<'static>>, width: u16, base: Style) -> Line<'static> {
    let used: usize = spans.iter().map(|span| span.content.width()).sum();
    let width = usize::from(width);
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), base));
    }
    Line::from(spans).style(base)
}

fn rollback_rule(width: u16, theme: &Theme) -> Line<'static> {
    let width = usize::from(width);
    let label = if width < 34 {
        " ⇣ rolls back below "
    } else {
        " ⇣ rolls back below on submit "
    };
    let right = width.saturating_sub(2 + label.width());
    let rule = format!("──{label}{}", "─".repeat(right));
    Line::from(Span::styled(
        rule.chars().take(width).collect::<String>(),
        theme.accent_coral.add_modifier(Modifier::DIM),
    ))
}

/// The prompt's text in the card, wrapped to it, with the caret drawn as a block.
fn edit_rows(
    edit: &EditView,
    width: u16,
    first_row: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, (usize, u16)) {
    let card = theme.rewind_surface;
    let rail = theme.accent_amber.patch(card);
    let caret_style = Style::default()
        .fg(card.bg.unwrap_or_default())
        .bg(theme.accent_amber.fg.unwrap_or_default())
        .add_modifier(Modifier::BOLD);
    let inner = usize::from(width.saturating_sub(2)).max(1);
    let wrapped = wrap_with_caret(&edit.text, edit.cursor.min(edit.text.len()), inner);
    let mut rows = Vec::with_capacity(wrapped.lines.len());
    for (row, text) in wrapped.lines.into_iter().enumerate() {
        let mut spans = vec![Span::styled("▌ ", rail)];
        if row == wrapped.row {
            let at = wrapped.byte.min(text.len());
            let (before, rest) = text.split_at(at);
            if !before.is_empty() {
                spans.push(Span::styled(before.to_owned(), card));
            }
            let mut chars = rest.chars();
            match chars.next() {
                Some(ch) => {
                    spans.push(Span::styled(ch.to_string(), caret_style));
                    if !chars.as_str().is_empty() {
                        spans.push(Span::styled(chars.as_str().to_owned(), card));
                    }
                }
                None => spans.push(Span::styled(" ", caret_style)),
            }
        } else {
            spans.push(Span::styled(text, card));
        }
        rows.push(pad(spans, width, card));
    }
    let column = 2 + u16::try_from(wrapped.column).unwrap_or(0);
    (rows, (first_row + wrapped.row, column))
}

struct Wrapped {
    lines: Vec<String>,
    row: usize,
    byte: usize,
    column: usize,
}

/// `text` cut into rows of `width` columns, and where byte offset `cursor` falls in them.
fn wrap_with_caret(text: &str, cursor: usize, width: usize) -> Wrapped {
    let mut lines = Vec::new();
    let (mut row, mut byte, mut column) = (0, 0, 0);
    let mut offset = 0;
    for raw in text.split('\n') {
        let end = offset + raw.len();
        let caret_here = (offset..=end).contains(&cursor);
        let mut consumed = 0;
        let chunks = chunks(raw, width);
        for (index, chunk) in chunks.iter().enumerate() {
            let last = index + 1 == chunks.len();
            let chunk_end = consumed + chunk.len();
            let relative = cursor.wrapping_sub(offset);
            if caret_here && relative >= consumed && (relative < chunk_end || last) {
                row = lines.len();
                byte = relative - consumed;
                column = chunk[..byte.min(chunk.len())].width();
            }
            lines.push(chunk.clone());
            consumed = chunk_end;
        }
        offset = end + 1;
    }
    Wrapped {
        lines,
        row,
        byte,
        column,
    }
}

/// `line` cut into pieces of at most `width` columns; an empty line is one empty piece.
fn chunks(line: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut used = 0;
    for ch in line.chars() {
        let columns = unicode_width::UnicodeWidthChar::width(ch)
            .unwrap_or(1)
            .max(1);
        if !current.is_empty() && used + columns > width {
            out.push(std::mem::take(&mut current));
            used = 0;
        }
        current.push(ch);
        used += columns;
    }
    out.push(current);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_caret_follows_the_cursor_through_wrapped_rows() {
        let wrapped = wrap_with_caret("abcdef\nxy", 5, 4);
        assert_eq!(wrapped.lines, vec!["abcd", "ef", "xy"]);
        assert_eq!((wrapped.row, wrapped.byte, wrapped.column), (1, 1, 1));
        let at_end = wrap_with_caret("abcd", 4, 4);
        assert_eq!((at_end.row, at_end.byte), (0, 4));
    }
}
