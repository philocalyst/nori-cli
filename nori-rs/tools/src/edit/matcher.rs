//! Locating `old_string` in a file, from strict to forgiving.
//!
//! Tiers, in order: exact; Unicode-folded; trailing-whitespace-insensitive whole lines;
//! indentation-insensitive whole lines. The first tier that finds any match decides; a later,
//! looser tier is never consulted to break a tie of an earlier one.

use serde::Deserialize;
use serde::Serialize;

use super::fold::FoldedText;
use super::fold::fold_str;
use super::indent::IndentPlan;
use super::indent::plan as plan_indent;
use super::lines::LineIndex;

/// Files above this size only get exact matching: the folded tiers build per-character tables.
const FUZZY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// How `old_string` was matched, strictest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchTier {
    Exact,
    /// Exact once CRLF endings are normalised.
    LineEndings,
    /// Equal after NFC and folding curly quotes, dashes and non-breaking spaces.
    Unicode,
    /// Equal line by line ignoring trailing whitespace.
    TrailingWhitespace,
    /// Equal line by line ignoring indentation; the new text was re-indented.
    Indentation,
}

impl MatchTier {
    pub fn describe(self) -> &'static str {
        match self {
            Self::Exact => "matched exactly",
            Self::LineEndings => {
                "matched after normalising line endings (the file's CRLF/LF endings were kept)"
            }
            Self::Unicode => "matched ignoring typographic quotes, dashes and non-breaking spaces",
            Self::TrailingWhitespace => "matched ignoring trailing whitespace",
            Self::Indentation => "matched ignoring indentation",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Match {
    pub start: usize,
    pub end: usize,
}

pub fn find(text: &str, index: &LineIndex, old: &str) -> Option<(MatchTier, Vec<Match>)> {
    let exact: Vec<Match> = text
        .match_indices(old)
        .map(|(start, found)| Match {
            start,
            end: start + found.len(),
        })
        .collect();
    if !exact.is_empty() {
        return Some((MatchTier::Exact, exact));
    }
    if text.len() > FUZZY_MAX_BYTES {
        return None;
    }

    if !(text.is_ascii() && old.is_ascii()) {
        let needle: Vec<char> = fold_str(old).chars().collect();
        let spans = FoldedText::new(text).find_all(&needle);
        if !spans.is_empty() {
            let matches = spans
                .into_iter()
                .map(|(start, end)| Match { start, end })
                .collect();
            return Some((MatchTier::Unicode, matches));
        }
    }

    let trailing = find_lines(text, index, old, |line| fold_str(line.trim_end()));
    if !trailing.is_empty() {
        return Some((MatchTier::TrailingWhitespace, trailing));
    }
    let indentation = find_lines(text, index, old, |line| fold_str(line.trim()));
    if !indentation.is_empty() {
        return Some((MatchTier::Indentation, indentation));
    }
    None
}

/// Whole-line matching: the needle's lines must equal consecutive file lines under `key`.
fn find_lines(
    text: &str,
    index: &LineIndex,
    old: &str,
    key: impl Fn(&str) -> String,
) -> Vec<Match> {
    let mut needle_lines: Vec<&str> = old.split('\n').collect();
    let trailing_newline = old.ends_with('\n');
    if trailing_newline {
        needle_lines.pop();
    }
    let needle_keys: Vec<String> = needle_lines.iter().map(|line| key(line)).collect();
    // A needle with no visible content would match anywhere.
    if needle_keys.iter().all(String::is_empty) {
        return Vec::new();
    }
    let file_keys: Vec<String> = index.lines(text).iter().map(|line| key(line)).collect();
    let count = needle_keys.len();

    let mut matches = Vec::new();
    let mut line = 0;
    while line + count <= file_keys.len() {
        if file_keys[line..line + count] == needle_keys[..] {
            let start = index.start(line);
            let mut end = index.end_of_text(text, line + count - 1);
            if trailing_newline && end < text.len() {
                end += 1;
            }
            matches.push(Match { start, end });
            line += count;
        } else {
            line += 1;
        }
    }
    matches
}

/// Re-plan indentation per match once the replacement text is known.
pub fn plan_indentation(
    text: &str,
    index: &LineIndex,
    old: &str,
    found: &Match,
    new_text: &str,
) -> IndentPlan {
    let mut needle_lines: Vec<&str> = old.split('\n').collect();
    if old.ends_with('\n') {
        needle_lines.pop();
    }
    let first = index.line_of(found.start);
    let file_lines: Vec<&str> = (first..first + needle_lines.len())
        .map(|line| index.text(text, line))
        .collect();
    plan_indent(&needle_lines, &file_lines, new_text)
}
