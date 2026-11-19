//! The text a child's turn ends with.

/// The text a turn ends with: what the child said after its last tool call, or, when it said
/// nothing after it, the last thing it said before.
#[derive(Default)]
pub(super) struct FinalText {
    after_last_tool: String,
    before_last_tool: String,
}

impl FinalText {
    pub(super) fn push(&mut self, text: &str) {
        self.after_last_tool.push_str(text);
    }

    pub(super) fn tool(&mut self) {
        if !self.after_last_tool.trim().is_empty() {
            self.before_last_tool = std::mem::take(&mut self.after_last_tool);
        }
        self.after_last_tool.clear();
    }

    /// The last line written since the last tool call, shortened, when there is one.
    pub(super) fn last_line(&self) -> Option<String> {
        let text = &self.after_last_tool;
        let mut start = text.len().saturating_sub(400);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let line = text[start..]
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())?;
        Some(line.trim().chars().take(160).collect())
    }

    pub(super) fn finish(self) -> String {
        if self.after_last_tool.trim().is_empty() {
            self.before_last_tool
        } else {
            self.after_last_tool
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_final_text_is_what_came_after_the_last_tool() {
        let mut text = FinalText::default();
        text.push("Looking at the files.");
        text.tool();
        text.push("They hold ");
        text.push("alpha.");
        assert_eq!(text.finish(), "They hold alpha.");

        let mut silent_after_tool = FinalText::default();
        silent_after_tool.push("Done: alpha.");
        silent_after_tool.tool();
        assert_eq!(silent_after_tool.finish(), "Done: alpha.");
    }
}
