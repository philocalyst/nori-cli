//! Where the text goes on screen: a pure function of the text, its chips and the width.
//!
//! Nothing here depends on the cursor, the scroll position or what was drawn before, so the same
//! text at the same width always lands in the same place: typing, pasting and moving never make
//! earlier rows reflow, and a resize reflows everything at once.
//!
//! Lines wrap at word boundaries the way prose is wrapped elsewhere in the TUI (first fit, words
//! longer than a row are cut). Whitespace that would cross the right edge hangs on its row
//! instead of starting the next one, and one column is kept free on the right so the cursor can
//! sit after the last character of a full row without the row wrapping under it.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::chips::Chips;

/// One screen row of text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    /// Bytes of the text on this row, without the newline that may end it.
    pub range: Range<usize>,
    /// Whether the row ends a line of the text (rather than wrapping onto the next row).
    pub line_end: bool,
}

/// The rows of a text at one width.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Layout {
    /// Columns text may fill on a row (the width minus the cursor's column).
    pub wrap: usize,
    pub rows: Vec<Row>,
}

impl Layout {
    /// Lay `text` out for an area `width` columns wide.
    pub fn new(text: &str, chips: &Chips, width: u16) -> Self {
        let wrap = usize::from(width.saturating_sub(1)).max(1);
        let mut rows = Vec::new();
        let mut line_start = 0;
        for line in text.split('\n') {
            wrap_line(line, line_start, chips, wrap, &mut rows);
            line_start += line.len() + 1;
        }
        Layout { wrap, rows }
    }

    pub fn height(&self) -> usize {
        self.rows.len().max(1)
    }

    /// The row and column where byte offset `at` of `text` is drawn.
    pub fn position(&self, text: &str, chips: &Chips, at: usize) -> (usize, usize) {
        let row = self.row_of(at);
        let Some(range) = self.rows.get(row).map(|row| row.range.clone()) else {
            return (0, 0);
        };
        let end = at.clamp(range.start, range.end);
        let col = columns(&text[range.start..end], chips);
        (row, col)
    }

    /// The row byte offset `at` is on. An offset where one row wraps into the next belongs to
    /// the next row, except at the end of a line.
    pub fn row_of(&self, at: usize) -> usize {
        let after = self.rows.partition_point(|row| row.range.start <= at);
        let row = after.saturating_sub(1);
        // `at` past the end of a wrapped row (its hanging whitespace end) still belongs to it.
        row.min(self.rows.len().saturating_sub(1))
    }

    /// The byte offset drawn at `col` on `row`, or the nearest one to its left; the end of the
    /// row when `col` is past it.
    pub fn offset_at(&self, text: &str, chips: &Chips, row: usize, col: usize) -> usize {
        let Some(row) = self.rows.get(row) else {
            return text.len();
        };
        let mut used = 0;
        for (at, grapheme) in text[row.range.clone()].grapheme_indices(true) {
            let width = grapheme_width(grapheme, chips);
            if used + width > col {
                return row.range.start + at;
            }
            used += width;
        }
        // Past the end: the end of a line, or the last grapheme of a wrapped row (its end is
        // where the next row starts).
        if row.line_end {
            row.range.end
        } else {
            text[row.range.clone()]
                .grapheme_indices(true)
                .next_back()
                .map_or(row.range.end, |(at, _)| row.range.start + at)
        }
    }
}

fn wrap_line(line: &str, start: usize, chips: &Chips, wrap: usize, rows: &mut Vec<Row>) {
    let mut row_start = 0;
    let mut used = 0;
    // A place the row may end: just after whitespace.
    let mut break_at: Option<usize> = None;
    let mut used_at_break = 0;
    for (at, grapheme) in line.grapheme_indices(true) {
        let width = grapheme_width(grapheme, chips);
        let blank = grapheme.chars().all(char::is_whitespace);
        if blank {
            // Whitespace never starts a row: it hangs off the end of this one.
            used += width;
            break_at = Some(at + grapheme.len());
            used_at_break = used;
            continue;
        }
        if used + width > wrap && at > row_start {
            let end = match break_at {
                Some(end) if end > row_start => end,
                Some(_) | None => at,
            };
            rows.push(Row {
                range: start + row_start..start + end,
                line_end: false,
            });
            used = if end == at { 0 } else { used - used_at_break };
            row_start = end;
            break_at = None;
        }
        used += width;
    }
    rows.push(Row {
        range: start + row_start..start + line.len(),
        line_end: true,
    });
}

fn grapheme_width(grapheme: &str, chips: &Chips) -> usize {
    let mut chars = grapheme.chars();
    if let (Some(ch), None) = (chars.next(), chars.next())
        && let Some(chip) = chips.of_char(ch)
    {
        return chip.width();
    }
    if grapheme == "\t" {
        return 4;
    }
    unicode_width::UnicodeWidthStr::width(grapheme)
}

/// Columns `text` takes on screen.
pub fn columns(text: &str, chips: &Chips) -> usize {
    text.graphemes(true)
        .map(|grapheme| grapheme_width(grapheme, chips))
        .sum()
}

/// Width of one grapheme as drawn.
pub(crate) fn width_of(grapheme: &str, chips: &Chips) -> usize {
    grapheme_width(grapheme, chips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn rows(text: &str, width: u16) -> Vec<&str> {
        let layout = Layout::new(text, &Chips::default(), width);
        layout
            .rows
            .iter()
            .map(|row| &text[row.range.clone()])
            .collect()
    }

    #[test]
    fn words_wrap_and_whitespace_hangs() {
        assert_eq!(rows("hello world again", 12), vec!["hello world ", "again"]);
        assert_eq!(rows("hello world", 6), vec!["hello ", "world"]);
    }

    #[test]
    fn long_words_are_cut() {
        assert_eq!(rows("abcdefghij", 5), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn newlines_end_rows_and_empty_lines_are_rows() {
        assert_eq!(rows("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(rows("", 10), vec![""]);
        assert_eq!(rows("a\n", 10), vec!["a", ""]);
    }

    #[test]
    fn the_cursor_column_is_kept_free() {
        // Four columns wide: three for text, one for a cursor after it.
        assert_eq!(rows("abc", 4), vec!["abc"]);
        let text = "abc";
        let layout = Layout::new(text, &Chips::default(), 4);
        assert_eq!(layout.position(text, &Chips::default(), 3), (0, 3));
    }

    #[test]
    fn positions_round_trip() {
        let text = "hello world again";
        let chips = Chips::default();
        let layout = Layout::new(text, &chips, 12);
        assert_eq!(layout.position(text, &chips, 12), (1, 0));
        assert_eq!(layout.position(text, &chips, 6), (0, 6));
        assert_eq!(layout.offset_at(text, &chips, 1, 2), 14);
        assert_eq!(layout.offset_at(text, &chips, 1, 40), text.len());
        assert_eq!(layout.offset_at(text, &chips, 0, 40), 11);
    }
}
