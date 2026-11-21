//! Applying an updated file's chunks to its text.

use crate::edit::Document;
use crate::edit::diagnose_text;
use crate::edit::fold_for_compare;
use crate::edit::reindent_lines;

use super::Chunk;
use super::PatchError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strictness {
    Exact,
    TrailingWhitespace,
    Trim,
}

fn same(strictness: Strictness, a: &str, b: &str) -> bool {
    match strictness {
        Strictness::Exact => a == b,
        Strictness::TrailingWhitespace => {
            fold_for_compare(a.trim_end()) == fold_for_compare(b.trim_end())
        }
        Strictness::Trim => fold_for_compare(a.trim()) == fold_for_compare(b.trim()),
    }
}

/// Find `pattern` in `lines` at or after `start`, loosening the comparison step by step.
fn seek(
    lines: &[String],
    pattern: &[String],
    start: usize,
    at_end: bool,
) -> Option<(usize, Strictness)> {
    if pattern.len() > lines.len() {
        return None;
    }
    let last_start = lines.len() - pattern.len();
    for strictness in [
        Strictness::Exact,
        Strictness::TrailingWhitespace,
        Strictness::Trim,
    ] {
        let hit = |at: usize| {
            pattern
                .iter()
                .zip(&lines[at..])
                .all(|(wanted, line)| same(strictness, line, wanted))
        };
        if at_end && hit(last_start) {
            return Some((last_start, strictness));
        }
        if let Some(found) = (start..=last_start).find(|at| hit(*at)) {
            return Some((found, strictness));
        }
    }
    None
}

/// Apply `chunks` to `original`, keeping its line endings and BOM. Returns the new text.
pub(super) fn apply_chunks(
    path: &str,
    original: &str,
    chunks: &[Chunk],
) -> Result<String, PatchError> {
    let mut doc = Document::parse(original);
    let mut lines: Vec<String> = doc.lf().split('\n').map(ToOwned::to_owned).collect();
    if doc.lf().ends_with('\n') || doc.lf().is_empty() {
        lines.pop();
    }
    let mut cursor = 0;

    for (number, chunk) in chunks.iter().enumerate() {
        for context in &chunk.contexts {
            let wanted = [context.clone()];
            let Some((at, _)) = seek(&lines, &wanted, cursor, false) else {
                return Err(PatchError::ContextNotFound {
                    path: path.to_owned(),
                    context: context.clone(),
                });
            };
            cursor = at + 1;
        }

        if chunk.old_lines.is_empty() {
            let at = if chunk.contexts.is_empty() || chunk.at_end_of_file {
                lines.len()
            } else {
                cursor
            };
            let added = chunk.new_lines.clone();
            cursor = at + added.len();
            lines.splice(at..at, added);
            continue;
        }

        // A trailing blank old line is the file's final newline marker; retry without it.
        let mut pattern = chunk.old_lines.clone();
        let mut replacement = chunk.new_lines.clone();
        let mut found = seek(&lines, &pattern, cursor, chunk.at_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern.pop();
            if replacement.last().is_some_and(String::is_empty) {
                replacement.pop();
            }
            found = seek(&lines, &pattern, cursor, chunk.at_end_of_file);
        }
        let Some((at, strictness)) = found else {
            let old = chunk.old_lines.join("\n");
            return Err(PatchError::HunkNotFound {
                path: path.to_owned(),
                hunk: i64::try_from(number + 1).unwrap_or(i64::MAX),
                hint: diagnose_text(doc.lf(), &old),
            });
        };
        if strictness == Strictness::Trim {
            let file: Vec<&str> = lines[at..at + pattern.len()]
                .iter()
                .map(String::as_str)
                .collect();
            let wanted: Vec<&str> = pattern.iter().map(String::as_str).collect();
            let new_text = replacement.join("\n");
            replacement = reindent_lines(&wanted, &file, &new_text)
                .split('\n')
                .map(ToOwned::to_owned)
                .collect();
            if replacement.len() == 1 && replacement[0].is_empty() && chunk.new_lines.is_empty() {
                replacement.clear();
            }
        }
        let count = pattern.len();
        cursor = at + replacement.len();
        lines.splice(at..at + count, replacement);
    }

    let mut text = lines.join("\n");
    if !lines.is_empty() {
        text.push('\n');
    }
    doc.set_all(&text);
    Ok(doc.render())
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn chunk(contexts: &[&str], old: &[&str], new: &[&str]) -> Chunk {
        Chunk {
            contexts: contexts.iter().map(ToString::to_string).collect(),
            old_lines: old.iter().map(ToString::to_string).collect(),
            new_lines: new.iter().map(ToString::to_string).collect(),
            at_end_of_file: false,
        }
    }

    #[test]
    fn replaces_lines_and_keeps_crlf() {
        let out = apply_chunks("f", "a\r\nb\r\nc\r\n", &[chunk(&[], &["b"], &["B1", "B2"])])
            .expect("apply");
        assert_eq!(out, "a\r\nB1\r\nB2\r\nc\r\n");
    }

    #[test]
    fn context_selects_between_repeated_blocks() {
        let original = "fn a() {\n    x\n}\nfn b() {\n    x\n}\n";
        let out = apply_chunks(
            "f",
            original,
            &[chunk(&["fn b() {"], &["    x"], &["    y"])],
        )
        .expect("apply");
        assert_eq!(out, "fn a() {\n    x\n}\nfn b() {\n    y\n}\n");
    }

    #[test]
    fn indentation_differences_are_reindented() {
        let original = "class A:\n    def f(self):\n        return 1\n";
        let out = apply_chunks(
            "f",
            original,
            &[chunk(
                &[],
                &["def f(self):", "    return 1"],
                &["def f(self):", "    return 2"],
            )],
        )
        .expect("apply");
        assert_eq!(out, "class A:\n    def f(self):\n        return 2\n");
    }

    #[test]
    fn a_missing_hunk_reports_the_closest_lines() {
        let err = apply_chunks("f", "alpha\nbeta\n", &[chunk(&[], &["gamma"], &["x"])])
            .expect_err("miss");
        assert!(matches!(err, PatchError::HunkNotFound { .. }));
    }

    #[test]
    fn pure_additions_append_or_follow_the_context() {
        let out = apply_chunks("f", "a\nb\n", &[chunk(&[], &[], &["c"])]).expect("apply");
        assert_eq!(out, "a\nb\nc\n");
        let out = apply_chunks("f", "a\nb\n", &[chunk(&["a"], &[], &["a2"])]).expect("apply");
        assert_eq!(out, "a\na2\nb\n");
    }
}
