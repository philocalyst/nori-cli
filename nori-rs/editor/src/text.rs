//! The buffer's text as a `String`, with the offsets needed to move between modalkit's
//! line/column cursors and byte offsets.

use modalkit::editing::cursor::Cursor;
use modalkit::editing::rope::EditRope;

/// The text of a buffer (without the newline modalkit keeps at its end) and where each line
/// starts.
#[derive(Clone, Debug)]
pub(crate) struct Text {
    pub(crate) text: String,
    line_starts: Vec<usize>,
}

impl Default for Text {
    fn default() -> Self {
        Self::new(String::new())
    }
}

impl Text {
    pub(crate) fn of_rope(rope: &EditRope) -> Self {
        let mut text = rope.to_string();
        // modalkit buffers always end with a newline of their own.
        if text.ends_with('\n') {
            text.pop();
        }
        Self::new(text)
    }

    pub(crate) fn new(text: String) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        Text { text, line_starts }
    }

    /// The byte offset of a modalkit cursor (line, column in characters).
    pub(crate) fn offset(&self, cursor: &Cursor) -> usize {
        let Some(&start) = self.line_starts.get(cursor.y) else {
            return self.text.len();
        };
        let end = self
            .line_starts
            .get(cursor.y + 1)
            .map_or(self.text.len(), |next| next - 1);
        let line = &self.text[start..end];
        start
            + line
                .char_indices()
                .nth(cursor.x)
                .map_or(line.len(), |(at, _)| at)
    }

    /// The modalkit cursor of byte offset `at` (clamped to a character boundary).
    pub(crate) fn cursor(&self, at: usize) -> Cursor {
        let mut at = at.min(self.text.len());
        while !self.text.is_char_boundary(at) {
            at -= 1;
        }
        let line = self.line_starts.partition_point(|start| *start <= at) - 1;
        let column = self.text[self.line_starts[line]..at].chars().count();
        Cursor::new(line, column)
    }

    /// The start and end of the line `at` is on (without its newline).
    pub(crate) fn line_bounds(&self, at: usize) -> (usize, usize) {
        let line = self.line_starts.partition_point(|start| *start <= at) - 1;
        let start = self.line_starts[line];
        let end = self
            .line_starts
            .get(line + 1)
            .map_or(self.text.len(), |next| next - 1);
        (start, end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn offsets_and_cursors_agree() {
        let text = Text::new("héllo\nwörld".to_owned());
        assert_eq!(text.offset(&Cursor::new(0, 2)), 3);
        assert_eq!(text.offset(&Cursor::new(1, 2)), 10);
        assert_eq!(text.cursor(10), Cursor::new(1, 2));
        assert_eq!(text.cursor(6), Cursor::new(0, 5));
        assert_eq!(text.line_bounds(8), (7, 13));
    }
}
