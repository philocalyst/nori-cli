//! Line addressing over an LF-normalised string.

/// Byte offsets of every line start in a string.
#[derive(Debug)]
pub struct LineIndex {
    starts: Vec<usize>,
    len: usize,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(index, _)| index + 1));
        // A trailing newline does not start another line.
        if text.ends_with('\n') {
            starts.pop();
        }
        if text.is_empty() {
            starts.clear();
        }
        Self {
            starts,
            len: text.len(),
        }
    }

    pub fn line_count(&self) -> usize {
        self.starts.len()
    }

    /// The 0-based line containing byte `offset`.
    pub fn line_of(&self, offset: usize) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index.saturating_sub(1),
        }
    }

    pub fn start(&self, line: usize) -> usize {
        self.starts[line]
    }

    /// The byte offset just past the line's text, before its `\n`.
    pub fn end_of_text(&self, text: &str, line: usize) -> usize {
        match self.starts.get(line + 1) {
            Some(next) => next - 1,
            None => {
                if text.ends_with('\n') {
                    self.len - 1
                } else {
                    self.len
                }
            }
        }
    }

    pub fn text<'a>(&self, text: &'a str, line: usize) -> &'a str {
        &text[self.start(line)..self.end_of_text(text, line)]
    }

    pub fn lines<'a>(&self, text: &'a str) -> Vec<&'a str> {
        (0..self.line_count())
            .map(|line| self.text(text, line))
            .collect()
    }
}

pub fn count_newlines(text: &str) -> usize {
    text.bytes().filter(|byte| *byte == b'\n').count()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn indexes_lines_with_and_without_trailing_newline() {
        let text = "a\nbb\nccc\n";
        let index = LineIndex::new(text);
        assert_eq!(index.lines(text), vec!["a", "bb", "ccc"]);
        assert_eq!(index.line_of(3), 1);
        let text = "a\nbb";
        let index = LineIndex::new(text);
        assert_eq!(index.lines(text), vec!["a", "bb"]);
        assert_eq!(LineIndex::new("").line_count(), 0);
    }
}
