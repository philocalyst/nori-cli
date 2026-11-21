// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy
// of the MPL was not distributed with this file, You can obtain one at
// https://mozilla.org/MPL/2.0/.
//
// Derived from Helix (https://github.com/helix-editor/helix), `helix-core/src/movement.rs` and
// `helix-core/src/chars.rs`: the word-motion algorithm, rewritten for Nori's editor over a slice
// of characters and modalkit's inclusive selections. Modified for Nori.

//! Helix's word motions (`w`, `b`, `e`, `W`, `B`, `E`): each selects from where it starts to the
//! word boundary it reaches, so `w` then `d` deletes a word and the space after it.

use modalkit::editing::context::EditContext;
use modalkit::editing::context::Resolve;
use modalkit::editing::cursor::CursorState;
use modalkit::env::helix::WordMotion;
use modalkit::prelude::Count;
use modalkit::prelude::TargetShape;

use crate::editor::Editor;
use crate::editor::Selection;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Category {
    Whitespace,
    Eol,
    Word,
    Punctuation,
}

fn categorize(ch: char) -> Category {
    if ch == '\n' || ch == '\r' {
        Category::Eol
    } else if ch.is_whitespace() {
        Category::Whitespace
    } else if ch.is_alphanumeric() || ch == '_' {
        Category::Word
    } else {
        Category::Punctuation
    }
}

fn is_word_boundary(a: char, b: char) -> bool {
    categorize(a) != categorize(b)
}

fn is_long_word_boundary(a: char, b: char) -> bool {
    match (categorize(a), categorize(b)) {
        (Category::Word, Category::Punctuation) | (Category::Punctuation, Category::Word) => false,
        (a, b) => a != b,
    }
}

fn is_eol(ch: char) -> bool {
    categorize(ch) == Category::Eol
}

fn is_prev(motion: WordMotion) -> bool {
    match motion {
        WordMotion::PrevWordStart | WordMotion::PrevLongWordStart => true,
        WordMotion::NextWordStart
        | WordMotion::NextWordEnd
        | WordMotion::NextLongWordStart
        | WordMotion::NextLongWordEnd => false,
    }
}

fn reached_target(motion: WordMotion, prev: char, next: char) -> bool {
    match motion {
        WordMotion::NextWordStart => {
            is_word_boundary(prev, next) && (is_eol(next) || !next.is_whitespace())
        }
        WordMotion::NextWordEnd | WordMotion::PrevWordStart => {
            is_word_boundary(prev, next) && (!prev.is_whitespace() || is_eol(next))
        }
        WordMotion::NextLongWordStart => {
            is_long_word_boundary(prev, next) && (is_eol(next) || !next.is_whitespace())
        }
        WordMotion::NextLongWordEnd | WordMotion::PrevLongWordStart => {
            is_long_word_boundary(prev, next) && (!prev.is_whitespace() || is_eol(next))
        }
    }
}

/// A cursor over `chars` that can run backwards: `next` reads the character the motion meets
/// next, `prev` steps back over it.
struct Chars<'a> {
    chars: &'a [char],
    at: usize,
    reversed: bool,
}

impl Chars<'_> {
    fn next(&mut self) -> Option<char> {
        if self.reversed {
            let ch = *self.chars.get(self.at.checked_sub(1)?)?;
            self.at -= 1;
            Some(ch)
        } else {
            let ch = *self.chars.get(self.at)?;
            self.at += 1;
            Some(ch)
        }
    }

    fn prev(&mut self) -> Option<char> {
        if self.reversed {
            let ch = *self.chars.get(self.at)?;
            self.at += 1;
            Some(ch)
        } else {
            let ch = *self.chars.get(self.at.checked_sub(1)?)?;
            self.at -= 1;
            Some(ch)
        }
    }
}

/// One step of a motion from the end-exclusive range `(anchor, head)`.
fn range_to_target(chars: &[char], motion: WordMotion, origin: (usize, usize)) -> (usize, usize) {
    let reversed = is_prev(motion);
    let advance = |at: usize| {
        if reversed {
            at.saturating_sub(1)
        } else {
            at + 1
        }
    };
    let mut iter = Chars {
        chars,
        at: origin.1,
        reversed,
    };
    let (mut anchor, mut head) = origin;
    let mut prev_ch = {
        let ch = iter.prev();
        if ch.is_some() {
            iter.next();
        }
        ch
    };
    // Skip any newlines the motion starts on.
    while let Some(ch) = iter.next() {
        if is_eol(ch) {
            prev_ch = Some(ch);
            head = advance(head);
        } else {
            iter.prev();
            break;
        }
    }
    if prev_ch.is_some_and(is_eol) {
        anchor = head;
    }
    let head_start = head;
    while let Some(next_ch) = iter.next() {
        let reached = match prev_ch {
            None => true,
            Some(prev) => reached_target(motion, prev, next_ch),
        };
        if reached {
            if head == head_start {
                anchor = head;
            } else {
                break;
            }
        }
        prev_ch = Some(next_ch);
        head = advance(head);
    }
    (anchor, head)
}

/// Apply `motion` `count` times to the end-exclusive range `(anchor, head)` of `chars`.
fn word_move(
    chars: &[char],
    range: (usize, usize),
    count: usize,
    motion: WordMotion,
) -> (usize, usize) {
    let (anchor, head) = range;
    let prev = is_prev(motion);
    if (prev && head == 0) || (!prev && head >= chars.len()) {
        return range;
    }
    // Block-cursor semantics: start from the character under the cursor.
    let mut range = if prev {
        if anchor < head {
            (head, head - 1)
        } else {
            ((head + 1).min(chars.len()), head)
        }
    } else if anchor < head {
        (head - 1, head)
    } else {
        (head, (head + 1).min(chars.len()))
    };
    for _ in 0..count.max(1) {
        let next = range_to_target(chars, motion, range);
        if next == range {
            break;
        }
        range = next;
    }
    range
}

impl Editor {
    /// Carry out a Helix word motion at every cursor: select to the boundary, or with `extend`
    /// stretch each selection to it.
    pub(crate) fn word_motion(&mut self, motion: WordMotion, extend: bool, ctx: &EditContext) {
        let count = ctx.resolve(&Count::Contextual);
        let text = self.text.text.clone();
        let chars: Vec<char> = text.chars().collect();
        // Byte offset of each character, and of the end.
        let mut bytes: Vec<usize> = text.char_indices().map(|(at, _)| at).collect();
        bytes.push(text.len());
        let char_of = |byte: usize| bytes.partition_point(|at| *at < byte);

        let moved: Vec<CursorState> = self
            .selections()
            .iter()
            .map(|selection| {
                let Selection { anchor, head, .. } = *selection;
                let (a, h) = (char_of(anchor), char_of(head));
                // modalkit selections include both ends; Helix ranges end before their head.
                let range = if a <= h { (a, h + 1) } else { (a + 1, h) };
                let (new_anchor, new_head) = word_move(&chars, range, count, motion);
                let (new_anchor, new_head) = if new_head > new_anchor {
                    (new_anchor, new_head - 1)
                } else {
                    (new_anchor.saturating_sub(1).max(new_head), new_head)
                };
                let last = chars.len().saturating_sub(1);
                let new_head = new_head.min(last);
                let anchor = if extend { a } else { new_anchor.min(last) };
                CursorState::Selection(
                    self.text.cursor(bytes[new_head]),
                    self.text.cursor(bytes[anchor.min(bytes.len() - 1)]),
                    TargetShape::CharWise,
                )
            })
            .collect();
        self.set_states(moved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn next_word_start_selects_the_word_and_its_space() {
        let text = chars("alpha beta gamma");
        assert_eq!(
            word_move(&text, (0, 1), 1, WordMotion::NextWordStart),
            (0, 6)
        );
        assert_eq!(
            word_move(&text, (0, 6), 1, WordMotion::NextWordStart),
            (6, 11)
        );
        assert_eq!(
            word_move(&text, (0, 1), 2, WordMotion::NextWordStart),
            (6, 11)
        );
    }

    #[test]
    fn next_word_end_and_previous_word_start() {
        let text = chars("alpha beta");
        assert_eq!(word_move(&text, (0, 1), 1, WordMotion::NextWordEnd), (0, 5));
        assert_eq!(
            word_move(&text, (9, 10), 1, WordMotion::PrevWordStart),
            (10, 6)
        );
    }

    #[test]
    fn long_words_cross_punctuation() {
        let text = chars("foo.bar baz");
        assert_eq!(
            word_move(&text, (0, 1), 1, WordMotion::NextWordStart),
            (0, 3)
        );
        assert_eq!(
            word_move(&text, (0, 1), 1, WordMotion::NextLongWordStart),
            (0, 8)
        );
    }
}
