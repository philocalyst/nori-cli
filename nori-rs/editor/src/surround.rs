//! Helix's surround edits (`ms`, `mr`, `md`), which modalkit's actions cannot express.

use std::ops::Range;

use modalkit::editing::cursor::CursorState;
use modalkit::prelude::TargetShape;

use crate::editor::Editor;

/// The opening and closing characters of the pair `ch` belongs to.
fn pair(ch: char) -> (char, char) {
    match ch {
        '(' | ')' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        other => (other, other),
    }
}

/// The nearest `open`/`close` pair around byte offset `at` of `text` (a pair character at `at`
/// counts as one of its ends): the byte offsets of the two characters.
fn enclosing(text: &str, at: usize, open: char, close: char) -> Option<(usize, usize)> {
    let here = text[at.min(text.len())..].chars().next();
    let symmetric = open == close;
    // Standing on a bracket: it is one end of the pair.
    let (before, after) = match here {
        Some(ch) if ch == open && !symmetric => (at + ch.len_utf8(), at + ch.len_utf8()),
        Some(ch) if ch == close && !symmetric => (at, at),
        Some(_) | None => (at, at),
    };
    let mut depth = 0;
    let mut start = None;
    for (offset, ch) in text[..before].char_indices().rev() {
        if ch == close && !symmetric {
            depth += 1;
        } else if ch == open {
            if depth == 0 {
                start = Some(offset);
                break;
            }
            depth -= 1;
        }
    }
    let start = start?;
    let mut depth = 0;
    let from = after.max(start + open.len_utf8());
    for (offset, ch) in text[from..].char_indices() {
        if ch == open && !symmetric {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                return Some((start, from + offset));
            }
            depth -= 1;
        }
    }
    None
}

impl Editor {
    pub(crate) fn surround_add(&mut self, ch: char) {
        let (open, close) = pair(ch);
        let text = self.text.text.clone();
        let mut ranges: Vec<Range<usize>> = self
            .selections()
            .iter()
            .map(|selection| selection.range(&text))
            .collect();
        ranges.sort_by_key(|range| range.start);
        let mut new = String::with_capacity(text.len() + ranges.len() * 2);
        let mut selected = Vec::with_capacity(ranges.len());
        let mut copied = 0;
        for range in ranges {
            if range.start < copied {
                continue;
            }
            new.push_str(&text[copied..range.start]);
            let start = new.len();
            new.push(open);
            new.push_str(&text[range.clone()]);
            new.push(close);
            selected.push(start..new.len());
            copied = range.end;
        }
        new.push_str(&text[copied..]);
        self.replace_with_selections(&new, &selected);
    }

    pub(crate) fn surround_delete(&mut self, ch: char) {
        self.surround_change(ch, None);
    }

    pub(crate) fn surround_replace(&mut self, from: char, to: char) {
        self.surround_change(from, Some(pair(to)));
    }

    /// Remove (or, with `to`, replace) the nearest `ch` pair around each selection.
    fn surround_change(&mut self, ch: char, to: Option<(char, char)>) {
        let (open, close) = pair(ch);
        let text = self.text.text.clone();
        let mut pairs: Vec<(usize, usize)> = self
            .selections()
            .iter()
            .filter_map(|selection| enclosing(&text, selection.head, open, close))
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        if pairs.is_empty() {
            self.events.push(crate::editor::EditorEvent::Notice(format!(
                "no surrounding {open}{close}"
            )));
            return;
        }
        let mut edits: Vec<(usize, usize, String)> = Vec::with_capacity(pairs.len() * 2);
        for (start, end) in &pairs {
            let (new_open, new_close) = match to {
                Some((o, c)) => (o.to_string(), c.to_string()),
                None => (String::new(), String::new()),
            };
            edits.push((*start, start + open.len_utf8(), new_open));
            edits.push((*end, end + close.len_utf8(), new_close));
        }
        edits.sort_by_key(|(start, ..)| *start);
        let mut new = String::with_capacity(text.len());
        let mut copied = 0;
        let mut selected = Vec::with_capacity(pairs.len());
        let mut pair_start = 0;
        for (index, (start, end, with)) in edits.iter().enumerate() {
            if *start < copied {
                continue;
            }
            new.push_str(&text[copied..*start]);
            if index % 2 == 0 {
                pair_start = new.len();
            }
            new.push_str(with);
            if index % 2 == 1 {
                selected.push(pair_start..new.len());
            }
            copied = *end;
        }
        new.push_str(&text[copied..]);
        self.replace_with_selections(&new, &selected);
    }

    /// Replace the text as one undoable edit, then select `ranges` (byte ranges of the new text).
    fn replace_with_selections(&mut self, text: &str, ranges: &[Range<usize>]) {
        let cursor = ranges.first().map_or(0, |range| range.start);
        self.replace_text(text, cursor);
        let states: Vec<CursorState> = ranges
            .iter()
            .filter(|range| !range.is_empty())
            .map(|range| {
                let last = self.text.text[..range.end]
                    .char_indices()
                    .next_back()
                    .map_or(range.start, |(at, _)| at);
                CursorState::Selection(
                    self.text.cursor(last),
                    self.text.cursor(range.start),
                    TargetShape::CharWise,
                )
            })
            .collect();
        self.set_states(states);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn finds_the_nearest_enclosing_pair() {
        let text = "f(a, (b), c)";
        assert_eq!(enclosing(text, 6, '(', ')'), Some((5, 7)));
        assert_eq!(enclosing(text, 2, '(', ')'), Some((1, 11)));
        // On a bracket: that bracket's pair.
        assert_eq!(enclosing(text, 11, '(', ')'), Some((1, 11)));
        assert_eq!(enclosing(text, 1, '(', ')'), Some((1, 11)));
        assert_eq!(enclosing("say \"hi\" now", 5, '"', '"'), Some((4, 7)));
        assert_eq!(enclosing("none", 1, '(', ')'), None);
    }
}
