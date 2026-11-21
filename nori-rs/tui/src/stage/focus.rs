//! Walking back to an earlier prompt, edited where it is.
//!
//! When the composer walks back to a prompt (↑ at its top), that prompt is edited in place: the
//! stage draws it as the prompt it always was, with the composer's editor in place of its text and
//! the cursor in it, and everything after it dimmed, so what sending from there would roll back
//! is plain before it is sent. There is no card, header or second bar, and no text box under the
//! conversation while the walk is on (see `bottom_pane::chat_composer::inline_walk`).

use std::sync::Arc;

use ratatui::style::Stylize;
use ratatui::text::Line;

use crate::history_cell::HistoryCell;

/// The walked prompt's text as the editor draws it, and where its cursor is: `(column, row)` in
/// `body`.
#[derive(Clone, Debug, Default)]
pub(crate) struct InlineEdit {
    pub(crate) body: Vec<Line<'static>>,
    pub(crate) cursor: Option<(u16, u16)>,
}

/// What the stage needs to draw the focused prompt and what follows it.
#[derive(Clone, Debug)]
pub(crate) struct FocusView {
    /// Index of the focused prompt in the transcript.
    pub(crate) selected: usize,
    /// The prompt as it is being edited, when it is edited in place.
    pub(crate) edit: Option<InlineEdit>,
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
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (index, cell) in cells.iter().enumerate() {
        if index > 0 && !cell.is_stream_continuation() {
            lines.push(Line::default());
        }
        let plain = super::rows::hard_wrap(cell.display_lines(width), width);
        match focus.rows(index, &plain, width) {
            Some(focused) => lines.extend(focused.lines),
            None => lines.extend(plain),
        }
    }
    FocusedColumn { lines }
}

/// A cell as drawn while a prompt is focused, and where the cursor is in its rows: `(row,
/// column)`.
pub(crate) struct FocusedRows {
    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) cursor: Option<(usize, u16)>,
}

impl FocusView {
    /// The rows of the cell at `index`, whose ordinary rows are `plain`, or `None` when the cell is
    /// drawn as it always is (it is before the focused prompt, or it is the prompt and is not
    /// edited here).
    pub(crate) fn rows(
        &self,
        index: usize,
        plain: &[Line<'static>],
        width: u16,
    ) -> Option<FocusedRows> {
        if index < self.selected {
            return None;
        }
        if index > self.selected {
            // What a rewind from the prompt would roll back.
            return Some(FocusedRows {
                lines: plain.iter().cloned().map(Stylize::dim).collect(),
                cursor: None,
            });
        }
        let edit = self.edit.as_ref()?;
        let (lines, first_body_row, indent) =
            crate::history_cell::frame_prompt(edit.body.clone(), Some(usize::from(width)));
        let cursor = edit.cursor.map(|(column, row)| {
            (
                first_body_row + usize::from(row),
                column.saturating_add(indent),
            )
        });
        Some(FocusedRows { lines, cursor })
    }
}
