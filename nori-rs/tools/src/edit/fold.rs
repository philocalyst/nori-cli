//! Folding text for forgiving comparison: NFC, typographic quotes, dashes and spaces.
//!
//! A model rarely reproduces a curly quote or a non-breaking space exactly, and a file written
//! on macOS may hold decomposed accents. Folding maps both sides onto one canonical form so the
//! edit can still land, while the original text is what gets replaced.

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::canonical_combining_class;

enum Folded {
    Drop,
    One(char),
    Text(&'static str),
}

fn fold_char(c: char) -> Folded {
    match c {
        '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' | '\u{2032}' => Folded::One('\''),
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' | '\u{2033}' => Folded::One('"'),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
        | '\u{2212}' => Folded::One('-'),
        '\u{2026}' => Folded::Text("..."),
        '\u{00A0}'
        | '\u{1680}'
        | '\u{2000}'..='\u{200A}'
        | '\u{202F}'
        | '\u{205F}'
        | '\u{3000}' => Folded::One(' '),
        '\u{200B}' | '\u{2060}' | '\u{FEFF}' => Folded::Drop,
        other => Folded::One(other),
    }
}

/// Fold a string. ASCII is returned unchanged without allocating a second time.
pub fn fold_str(s: &str) -> String {
    if s.is_ascii() {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.nfc() {
        match fold_char(c) {
            Folded::Drop => {}
            Folded::One(c) => out.push(c),
            Folded::Text(text) => out.push_str(text),
        }
    }
    out
}

/// A folded copy of some text with, for each folded char, the byte span it came from.
#[derive(Debug)]
pub struct FoldedText {
    pub chars: Vec<char>,
    pub spans: Vec<(usize, usize)>,
}

impl FoldedText {
    pub fn new(text: &str) -> Self {
        let mut chars = Vec::with_capacity(text.len());
        let mut spans = Vec::with_capacity(text.len());
        let mut segment_start = 0;
        let mut segment_end = 0;
        let flush =
            |start: usize, end: usize, chars: &mut Vec<char>, spans: &mut Vec<(usize, usize)>| {
                if start == end {
                    return;
                }
                for c in text[start..end].nfc() {
                    match fold_char(c) {
                        Folded::Drop => {}
                        Folded::One(c) => {
                            chars.push(c);
                            spans.push((start, end));
                        }
                        Folded::Text(replacement) => {
                            for c in replacement.chars() {
                                chars.push(c);
                                spans.push((start, end));
                            }
                        }
                    }
                }
            };
        for (index, c) in text.char_indices() {
            let starts_segment = canonical_combining_class(c) == 0;
            if starts_segment {
                flush(segment_start, segment_end, &mut chars, &mut spans);
                segment_start = index;
            }
            segment_end = index + c.len_utf8();
        }
        flush(segment_start, segment_end, &mut chars, &mut spans);
        Self { chars, spans }
    }

    /// Byte ranges (in the original text) of every non-overlapping occurrence of `needle`.
    pub fn find_all(&self, needle: &[char]) -> Vec<(usize, usize)> {
        let mut found = Vec::new();
        let Some(first) = needle.first() else {
            return found;
        };
        let mut i = 0;
        while i + needle.len() <= self.chars.len() {
            if self.chars[i] == *first && self.chars[i..i + needle.len()] == *needle {
                let start = self.spans[i].0;
                let end = self.spans[i + needle.len() - 1].1;
                if found
                    .last()
                    .is_none_or(|(_, previous_end)| start >= *previous_end)
                {
                    found.push((start, end));
                }
                i += needle.len();
            } else {
                i += 1;
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn folds_quotes_dashes_spaces_and_ellipsis() {
        assert_eq!(
            fold_str("\u{201C}a\u{201D} \u{2014} it\u{2019}s\u{00A0}ok\u{2026}"),
            "\"a\" - it's ok..."
        );
    }

    #[test]
    fn composes_decomposed_accents() {
        assert_eq!(fold_str("cafe\u{0301}"), "caf\u{00E9}");
    }

    #[test]
    fn expanded_characters_do_not_produce_overlapping_ranges() {
        let folded = FoldedText::new("…");
        assert_eq!(folded.find_all(&['.']), vec![(0, "…".len())]);
    }

    #[test]
    fn find_all_maps_back_to_original_bytes() {
        let text = "x \u{201C}hi\u{201D} y";
        let folded = FoldedText::new(text);
        let needle: Vec<char> = "\"hi\"".chars().collect();
        let spans = folded.find_all(&needle);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].0..spans[0].1], "\u{201C}hi\u{201D}");
    }
}
