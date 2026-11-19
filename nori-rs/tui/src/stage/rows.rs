//! Cells as hard-wrapped rows, cached per width.
//!
//! The stage draws cells into a cell grid, so every logical line has to become rows no wider than
//! the screen. A cell's rows only change when the cell is replaced, when it is expanded or
//! collapsed, or when the width changes, so they are kept until then.

use std::collections::HashMap;
use std::sync::Arc;

use ratatui::text::Line;
use ratatui::text::Span;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::history_cell::HistoryCell;

/// The rows of one cell.
#[derive(Debug)]
pub(crate) struct CellRows {
    pub(crate) lines: Vec<Line<'static>>,
    /// Whether the cell can be expanded, and if so whether it is. Clicking a card or `Alt+E`
    /// flips it with `HistoryCell::set_expanded`; the cell draws itself accordingly.
    pub(crate) expansion: Option<bool>,
}

struct Entry {
    /// Kept so a replaced cell can never be mistaken for the one the rows were made from.
    cell: Arc<dyn HistoryCell>,
    rows: Arc<CellRows>,
}

#[derive(Default)]
pub(crate) struct RowCache {
    width: u16,
    entries: HashMap<usize, Entry>,
}

impl RowCache {
    /// The rows of the cell at `index`, for `width`. They are made again when the cell was
    /// replaced, was expanded or collapsed, or changes with time alone.
    pub(crate) fn rows(
        &mut self,
        index: usize,
        cell: &Arc<dyn HistoryCell>,
        width: u16,
    ) -> Arc<CellRows> {
        if self.width != width {
            self.entries.clear();
            self.width = width;
        }
        let expansion = cell.expansion();
        if !cell.animated()
            && let Some(entry) = self.entries.get(&index)
            && Arc::ptr_eq(&entry.cell, cell)
            && entry.rows.expansion == expansion
        {
            return entry.rows.clone();
        }
        let rows = Arc::new(CellRows {
            lines: hard_wrap(cell.display_lines(width), width),
            expansion,
        });
        self.entries.insert(
            index,
            Entry {
                cell: cell.clone(),
                rows: rows.clone(),
            },
        );
        rows
    }

    /// Forget the cells before `index`: they are in the scrollback and are not drawn again.
    pub(crate) fn forget_before(&mut self, index: usize) {
        self.entries.retain(|key, _| *key + 4 >= index);
    }

    /// Forget the cells from `index` on: they changed.
    pub(crate) fn forget_from(&mut self, index: usize) {
        self.entries.retain(|key, _| *key < index);
    }
}

/// Rows no wider than `width` for `lines`. Words wrap where they can; a token that cannot (a URL)
/// is cut at the edge.
pub(crate) fn hard_wrap(lines: Vec<Line<'static>>, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let mut rows = Vec::with_capacity(lines.len());
    for line in lines {
        if line.width() <= width {
            rows.push(line);
            continue;
        }
        for wrapped in crate::wrapping::adaptive_wrap_line(&line, width) {
            if wrapped.width() <= width {
                rows.push(crate::render::line_utils::line_to_static(&wrapped));
            } else {
                rows.extend(cut_at_width(&wrapped, width));
            }
        }
    }
    rows
}

/// `line` cut into rows of `width` columns, a wide grapheme never split across two.
fn cut_at_width(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for span in &line.spans {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            let columns = grapheme.width();
            if used + columns > width && used > 0 {
                if !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), span.style));
                }
                rows.push(Line {
                    spans: std::mem::take(&mut spans),
                    ..line.clone()
                });
                used = 0;
            }
            text.push_str(grapheme);
            used += columns;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    if !spans.is_empty() || rows.is_empty() {
        rows.push(Line {
            spans,
            ..line.clone()
        });
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn text(rows: &[Line<'_>]) -> Vec<String> {
        rows.iter().map(Line::to_string).collect()
    }

    #[test]
    fn long_words_are_cut_and_short_lines_are_kept() {
        let rows = hard_wrap(
            vec![Line::from("short"), Line::from("https://example.com/a/b/c")],
            10,
        );
        assert_eq!(
            text(&rows),
            vec!["short", "https://ex", "ample.com/", "a/b/c"]
        );
    }

    #[test]
    fn a_wide_grapheme_moves_to_the_next_row() {
        let rows = cut_at_width(&Line::from("abc界xy"), 4);
        assert_eq!(text(&rows), vec!["abc", "界xy"]);
    }
}
