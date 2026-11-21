//! Where a streaming answer may be interrupted without cutting a paragraph.
//!
//! A cell that arrives while an answer streams (a subagent finished, a message between agents, the
//! results that woke the agent) goes into the scrollback where it happened only when the answer so
//! far ends at a paragraph: a blank line after text, outside a code fence. Anywhere else it waits
//! for the answer, so a sentence is never split.

/// Follows the text of a stream as deltas arrive, in constant space.
#[derive(Debug, Default)]
pub(crate) struct ParagraphBoundary {
    /// The line being typed.
    line: String,
    /// Whether the previous complete line was blank, after text.
    blank_after_text: bool,
    seen_text: bool,
    in_fence: bool,
}

impl ParagraphBoundary {
    pub(crate) fn push(&mut self, delta: &str) {
        for piece in delta.split_inclusive('\n') {
            self.line.push_str(piece);
            if !piece.ends_with('\n') {
                // A partial line: bounded, as only its start decides what the line is.
                if self.line.len() > 256 {
                    let mut end = 256;
                    while !self.line.is_char_boundary(end) {
                        end -= 1;
                    }
                    self.line.truncate(end);
                }
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = line.trim();
            if line.starts_with("```") || line.starts_with("~~~") {
                self.in_fence = !self.in_fence;
            }
            if line.is_empty() {
                self.blank_after_text = self.seen_text;
            } else {
                self.seen_text = true;
                self.blank_after_text = false;
            }
        }
    }

    /// Whether the text so far ends a paragraph and something else may go after it.
    pub(crate) fn at_paragraph_end(&self) -> bool {
        self.blank_after_text && self.line.is_empty() && !self.in_fence
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn only_a_blank_line_after_text_outside_a_fence_ends_a_paragraph() {
        let mut boundary = ParagraphBoundary::default();
        let mut ends = Vec::new();
        for delta in [
            "I'll split",
            " the audit.\n",
            "\n",
            "```rust\n",
            "let a = 1;\n",
            "\n",
            "```\n",
            "\n",
            "Next",
        ] {
            boundary.push(delta);
            ends.push(boundary.at_paragraph_end());
        }
        assert_eq!(
            ends,
            vec![false, false, true, false, false, false, false, true, false]
        );
    }

    #[test]
    fn truncates_long_unicode_lines_at_a_character_boundary() {
        let mut boundary = ParagraphBoundary::default();
        boundary.push(&format!("{}…", "a".repeat(255)));
        assert_eq!(boundary.line, "a".repeat(255));
        boundary.push("\n\n");
        assert!(boundary.at_paragraph_end());
    }
}
