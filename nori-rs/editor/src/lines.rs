//! Helix's line commands: `x` selects whole lines (and the next one when they already are), `X`
//! stretches each selection to whole lines, `>` and `<` indent and dedent the lines selections
//! touch.

use modalkit::editing::cursor::Cursor;
use modalkit::editing::cursor::CursorState;
use modalkit::prelude::TargetShape;

use crate::editor::Editor;

impl Editor {
    pub(crate) fn select_lines(&mut self, below: bool, count: usize) {
        let text = self.text.text.clone();
        let states: Vec<CursorState> = self
            .selections()
            .iter()
            .map(|selection| {
                let range = selection.range(&text);
                let (start, _) = self.text.line_bounds(range.start);
                let last = range.end.saturating_sub(1).max(range.start);
                let mut end = line_end_with_newline(&text, self.text.line_bounds(last).1);
                // Already whole lines: take the next `count` lines too.
                if below && range.start == start && range.end == end {
                    for _ in 0..count.max(1) {
                        if end >= text.len() {
                            break;
                        }
                        end = line_end_with_newline(&text, self.text.line_bounds(end).1);
                    }
                } else if below {
                    for _ in 1..count.max(1) {
                        if end >= text.len() {
                            break;
                        }
                        end = line_end_with_newline(&text, self.text.line_bounds(end).1);
                    }
                }
                // The selection's last character: the newline, or the last of the text.
                let head = text[..end]
                    .char_indices()
                    .next_back()
                    .map_or(start, |(at, _)| at);
                CursorState::Selection(
                    self.text.cursor(head),
                    self.text.cursor(start),
                    TargetShape::CharWise,
                )
            })
            .collect();
        self.set_states(states);
    }
}

/// What one level of indentation is.
const INDENT: &str = "    ";

impl Editor {
    /// Indent (or dedent) every line a selection touches by `count` levels, as one undoable edit.
    /// The selections stay on the same text.
    pub(crate) fn indent_lines(&mut self, more: bool, count: usize) {
        let text = self.text.text.clone();
        let mut starts: Vec<usize> = Vec::new();
        for selection in self.selections() {
            let range = selection.range(&text);
            let last = range.end.saturating_sub(1).max(range.start);
            let mut at = self.text.line_bounds(range.start).0;
            loop {
                starts.push(at);
                let (_, end) = self.text.line_bounds(at);
                if end >= last.max(at) || end >= text.len() {
                    break;
                }
                at = end + 1;
            }
        }
        starts.sort_unstable();
        starts.dedup();
        // Characters added (positive) or removed (negative) at the start of each line, by line.
        let mut deltas: std::collections::HashMap<usize, i64> = std::collections::HashMap::new();
        let mut new = String::with_capacity(text.len() + starts.len() * INDENT.len());
        let mut copied = 0;
        for start in starts {
            new.push_str(&text[copied..start]);
            copied = start;
            let line = self.text.cursor(start).y;
            let mut delta: i64 = 0;
            for _ in 0..count.max(1) {
                if more {
                    new.push_str(INDENT);
                    delta += INDENT.len() as i64;
                } else if text[copied..].starts_with('\t') {
                    copied += 1;
                    delta -= 1;
                } else {
                    let spaces = text[copied..]
                        .chars()
                        .take(INDENT.len())
                        .take_while(|ch| *ch == ' ')
                        .count();
                    copied += spaces;
                    delta -= spaces as i64;
                }
            }
            deltas.insert(line, delta);
        }
        new.push_str(&text[copied..]);
        if new == text {
            return;
        }
        let shift = |cursor: &Cursor| {
            let delta = deltas.get(&cursor.y).copied().unwrap_or(0);
            let x = (i64::try_from(cursor.x).unwrap_or(i64::MAX) + delta).max(0);
            Cursor::new(cursor.y, usize::try_from(x).unwrap_or(0))
        };
        let moved: Vec<CursorState> = self
            .group
            .iter()
            .map(|state| match state {
                CursorState::Location(cursor) => CursorState::Location(shift(cursor)),
                CursorState::Selection(cursor, anchor, shape) => {
                    CursorState::Selection(shift(cursor), shift(anchor), *shape)
                }
            })
            .collect();
        self.replace_text(&new, self.cursor());
        self.set_states(moved);
    }
}

/// The end of a line including its newline, given where its text ends.
fn line_end_with_newline(text: &str, end: usize) -> usize {
    if text[end..].starts_with('\n') {
        end + 1
    } else {
        end
    }
}
