//! A text file held as an LF view plus the line endings it came with.
//!
//! All matching happens on the LF view, so a CRLF file matches an `old_string` written with
//! plain newlines. The original ending of every newline is remembered and put back on render:
//! untouched lines keep theirs, and new lines take the file's dominant ending.

use std::iter::repeat_n;

use super::lines::count_newlines;

const BOM: char = '\u{FEFF}';

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    lf: String,
    /// One flag per `\n` in `lf`: true if it was `\r\n` in the file.
    crlf: Vec<bool>,
    bom: bool,
}

impl Document {
    pub fn parse(content: &str) -> Self {
        let (content, bom) = match content.strip_prefix(BOM) {
            Some(rest) => (rest, true),
            None => (content, false),
        };
        let mut lf = String::with_capacity(content.len());
        let mut crlf = Vec::new();
        let mut rest = content;
        while let Some(position) = rest.find('\n') {
            let (line, tail) = rest.split_at(position);
            match line.strip_suffix('\r') {
                Some(without_cr) => {
                    lf.push_str(without_cr);
                    crlf.push(true);
                }
                None => {
                    lf.push_str(line);
                    crlf.push(false);
                }
            }
            lf.push('\n');
            rest = &tail[1..];
        }
        lf.push_str(rest);
        Self { lf, crlf, bom }
    }

    pub fn lf(&self) -> &str {
        &self.lf
    }

    pub fn has_crlf(&self) -> bool {
        self.crlf.iter().any(|flag| *flag)
    }

    pub fn has_bom(&self) -> bool {
        self.bom
    }

    fn dominant_crlf(&self) -> bool {
        let crlf = self.crlf.iter().filter(|flag| **flag).count();
        crlf > self.crlf.len() - crlf
    }

    /// The file's bytes as text, with every line ending and the BOM put back.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.lf.len() + self.crlf.len() + 3);
        if self.bom {
            out.push(BOM);
        }
        let mut parts = self.lf.split('\n');
        if let Some(first) = parts.next() {
            out.push_str(first);
        }
        for (flag, part) in self.crlf.iter().zip(parts) {
            out.push_str(if *flag { "\r\n" } else { "\n" });
            out.push_str(part);
        }
        out
    }

    /// Replace byte ranges of the LF view. `replacements` must be sorted and disjoint. Where a
    /// replacement keeps the number of newlines, their endings are kept; otherwise the new
    /// newlines take the file's dominant ending.
    pub fn replace_ranges(&mut self, replacements: &[(usize, usize, String)]) {
        let dominant = self.dominant_crlf();
        let mut lf = String::with_capacity(self.lf.len());
        let mut crlf = Vec::with_capacity(self.crlf.len());
        let mut cursor = 0;
        let mut newline_cursor = 0;
        for (start, end, new) in replacements {
            let gap = &self.lf[cursor..*start];
            lf.push_str(gap);
            let gap_newlines = count_newlines(gap);
            crlf.extend_from_slice(&self.crlf[newline_cursor..newline_cursor + gap_newlines]);
            newline_cursor += gap_newlines;

            let replaced_newlines = count_newlines(&self.lf[*start..*end]);
            let new_newlines = count_newlines(new);
            lf.push_str(new);
            if new_newlines == replaced_newlines {
                crlf.extend_from_slice(
                    &self.crlf[newline_cursor..newline_cursor + replaced_newlines],
                );
            } else {
                crlf.extend(repeat_n(dominant, new_newlines));
            }
            newline_cursor += replaced_newlines;
            cursor = *end;
        }
        let tail = &self.lf[cursor..];
        lf.push_str(tail);
        crlf.extend_from_slice(&self.crlf[newline_cursor..]);
        self.lf = lf;
        self.crlf = crlf;
    }

    /// Replace the whole content with `text`, keeping the file's BOM and dominant ending.
    pub fn set_all(&mut self, text: &str) {
        let end = self.lf.len();
        self.replace_ranges(&[(0, end, text.to_owned())]);
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn parse_and_render_round_trip_every_ending_style() {
        for content in [
            "",
            "a",
            "a\n",
            "a\r\nb\r\n",
            "a\nb\r\nc",
            "\u{FEFF}a\r\n",
            "a\rb\n",
            "a\r\r\nb",
        ] {
            assert_eq!(Document::parse(content).render(), content, "{content:?}");
        }
    }

    #[test]
    fn lf_view_has_no_carriage_returns_before_newlines() {
        let doc = Document::parse("a\r\nb\nc\r\n");
        assert_eq!(doc.lf(), "a\nb\nc\n");
        assert!(doc.has_crlf());
    }

    #[test]
    fn unchanged_lines_keep_their_endings_and_new_lines_take_the_dominant_one() {
        let mut doc = Document::parse("a\r\nb\nc\r\nd\r\n");
        // Replace "c" with two lines.
        let start = doc.lf().find('c').expect("c");
        doc.replace_ranges(&[(start, start + 1, "c1\nc2".to_owned())]);
        assert_eq!(doc.render(), "a\r\nb\nc1\r\nc2\r\nd\r\n");
    }

    #[test]
    fn same_newline_count_keeps_original_endings() {
        let mut doc = Document::parse("a\nb\r\nc\n");
        let start = doc.lf().find('a').expect("a");
        doc.replace_ranges(&[(start, start + 3, "x\ny".to_owned())]);
        assert_eq!(doc.render(), "x\ny\r\nc\n");
    }
}
