//! The edit engine: pure, synchronous, tiered find-and-replace over text.
//!
//! Matching is strict first and forgiving in steps ([`MatchTier`]); a miss produces the closest
//! lines of the file ([`MissHint`]); an ambiguous match is refused with every candidate line.
//! Nothing here touches the filesystem: [`Document`] holds the text and its line endings, and
//! [`Document::apply_all`] applies a list of edits atomically.

mod apply;
mod diff;
mod document;
mod error;
pub(crate) mod fold;
mod hint;
pub(crate) mod indent;
mod lines;
mod matcher;

pub use apply::Edit;
pub use apply::EditReport;
pub use apply::strip_line_numbers;
pub use apply::to_lf;
pub use diff::count_changes;
pub use diff::unified_diff;
pub use document::Document;
pub use error::EditError;
pub use error::Site;
pub use hint::Candidate;
pub use hint::MissHint;
pub use lines::LineIndex;
pub use matcher::MatchTier;

/// The closest lines of `text` to `old`, for a hunk that did not match.
pub fn diagnose_text(text: &str, old: &str) -> MissHint {
    hint::diagnose(text, &LineIndex::new(text), old, "")
}

/// Re-indent `new_text`, written against `old` lines, to fit the matched `file` lines.
pub(crate) fn reindent_lines(old: &[&str], file: &[&str], new_text: &str) -> String {
    indent::plan(old, file, new_text).apply(new_text)
}

pub(crate) use fold::fold_str as fold_for_compare;

#[cfg(test)]
mod tests;
