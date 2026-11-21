//! Unified diffs and line counts.

use similar::ChangeTag;
use similar::TextDiff;

/// A unified diff with three lines of context and `a/` `b/` headers.
pub fn unified_diff(label: &str, old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{label}"), &format!("b/{label}"))
        .to_string()
}

/// `(lines added, lines removed)`.
pub fn count_changes(old: &str, new: &str) -> (i64, i64) {
    let mut added = 0;
    let mut removed = 0;
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn counts_and_renders() {
        assert_eq!(count_changes("a\nb\nc\n", "a\nB\nc\nd\n"), (2, 1));
        let diff = unified_diff("f.txt", "a\nb\n", "a\nB\n");
        assert!(diff.starts_with("--- a/f.txt\n+++ b/f.txt\n"), "{diff}");
        assert!(diff.contains("-b\n+B\n"), "{diff}");
    }
}
