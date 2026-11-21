//! The answer to "old_string was not found": the closest lines of the file and how they differ.

use std::fmt;

use similar::TextDiff;

use super::fold::fold_str;
use super::indent::describe_ws;
use super::indent::leading_ws;
use super::lines::LineIndex;

const SHOWN_LINES: usize = 5;
const MAX_COMPARED_LINES: usize = 12;
const MAX_TEXT_CHARS: usize = 120;
const MIN_ANCHOR_SIMILARITY: f32 = 0.4;
const MIN_WINDOW_SIMILARITY: f32 = 0.45;

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// 1-based number of the first line shown.
    pub first_line: i64,
    /// 0 to 100.
    pub similarity_percent: i64,
    pub shown: Vec<String>,
    pub differences: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MissHint {
    /// `new_string` is already in the file: the edit was probably applied before.
    pub already_applied: bool,
    pub candidate: Option<Candidate>,
}

impl fmt::Display for MissHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "old_string was not found in the file, so nothing was changed."
        )?;
        if self.already_applied {
            write!(
                f,
                "\nHowever, new_string already appears in the file: this edit may already have \
                 been applied. `read` the file to check before retrying."
            )?;
        }
        match &self.candidate {
            Some(candidate) => {
                let last =
                    candidate.first_line + i64::try_from(candidate.shown.len()).unwrap_or(0) - 1;
                write!(
                    f,
                    "\nClosest match in the file (lines {}-{last}, {}% similar):",
                    candidate.first_line, candidate.similarity_percent
                )?;
                for (offset, line) in candidate.shown.iter().enumerate() {
                    let number = candidate.first_line + i64::try_from(offset).unwrap_or(0);
                    write!(f, "\n{number:>6}→{line}")?;
                }
                if !candidate.differences.is_empty() {
                    write!(f, "\nHow your old_string differs:")?;
                    for difference in &candidate.differences {
                        write!(f, "\n  {difference}")?;
                    }
                }
                write!(
                    f,
                    "\nCopy old_string from the file text exactly (without the line-number \
                     prefixes) and retry. If the file may have changed, `read` it again first."
                )
            }
            None => write!(
                f,
                "\nNo similar lines were found. `read` the part of the file you want to change \
                 and copy old_string from it exactly (without the line-number prefixes)."
            ),
        }
    }
}

fn clip(line: &str) -> String {
    if line.chars().count() > MAX_TEXT_CHARS {
        let head: String = line.chars().take(MAX_TEXT_CHARS).collect();
        format!("{head}…")
    } else {
        line.to_owned()
    }
}

fn similarity(a: &str, b: &str) -> f32 {
    if a == b {
        return 1.0;
    }
    let (a_len, b_len) = (a.chars().count(), b.chars().count());
    if a_len == 0 || b_len == 0 {
        return 0.0;
    }
    TextDiff::from_chars(a, b).ratio()
}

fn key(line: &str) -> String {
    fold_str(line.trim())
}

fn why_differs(yours: &str, file: &str) -> Option<String> {
    if yours.trim_end() == file.trim_end() {
        None
    } else if yours.trim() == file.trim() {
        Some(format!(
            "only the indentation differs (yours: {}, file: {})",
            describe_ws_or_none(leading_ws(yours)),
            describe_ws_or_none(leading_ws(file))
        ))
    } else if fold_str(yours) == fold_str(file) {
        Some(
            "only typographic characters differ (curly quotes, dashes, non-breaking spaces)"
                .to_owned(),
        )
    } else {
        None
    }
}

fn describe_ws_or_none(ws: &str) -> String {
    if ws.is_empty() {
        "none".to_owned()
    } else {
        describe_ws(ws)
    }
}

/// Find the lines of `text` closest to `old` and explain the difference.
pub fn diagnose(text: &str, index: &LineIndex, old: &str, new: &str) -> MissHint {
    let already_applied = !new.trim().is_empty() && text.contains(new);
    MissHint {
        already_applied,
        candidate: closest(text, index, old),
    }
}

fn closest(text: &str, index: &LineIndex, old: &str) -> Option<Candidate> {
    let mut needle: Vec<&str> = old.split('\n').collect();
    if old.ends_with('\n') {
        needle.pop();
    }
    let anchor_offset = needle
        .iter()
        .position(|line| line.trim().chars().count() >= 4)
        .or_else(|| needle.iter().position(|line| !line.trim().is_empty()))?;
    let anchor_key = key(needle[anchor_offset]);
    let anchor_len = anchor_key.chars().count();
    let file_lines = index.lines(text);

    let mut anchors: Vec<(f32, usize)> = file_lines
        .iter()
        .enumerate()
        .filter_map(|(line, content)| {
            let line_key = key(content);
            let len = line_key.chars().count();
            if len * 2 < anchor_len || len > anchor_len * 2 + 8 {
                return None;
            }
            let score = similarity(&anchor_key, &line_key);
            (score >= MIN_ANCHOR_SIMILARITY).then_some((score, line))
        })
        .collect();
    anchors.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    anchors.truncate(4);

    let compared = needle.len().min(MAX_COMPARED_LINES);
    let mut best: Option<(f32, usize)> = None;
    for (_, anchor_line) in anchors {
        let start = anchor_line.saturating_sub(anchor_offset);
        let total: f32 = (0..compared)
            .map(|j| match file_lines.get(start + j) {
                Some(file) => similarity(&key(needle[j]), &key(file)),
                None => 0.0,
            })
            .sum();
        let mean = total / compared as f32;
        if best.is_none_or(|(score, _)| mean > score) {
            best = Some((mean, start));
        }
    }
    let (score, start) = best.filter(|(score, _)| *score >= MIN_WINDOW_SIMILARITY)?;

    let differing: Vec<usize> = (0..needle.len())
        .filter(|j| {
            file_lines
                .get(start + j)
                .is_some_and(|file| *file != needle[*j])
        })
        .collect();
    let first_diff = differing.first().copied().unwrap_or(0);
    let show_start = start + first_diff.saturating_sub(1);
    let window_end = (start + needle.len()).min(file_lines.len());
    let show_end = (show_start + SHOWN_LINES)
        .min(window_end.max(show_start + 1))
        .min(file_lines.len());
    let shown = file_lines[show_start..show_end]
        .iter()
        .map(|line| clip(line))
        .collect();

    let differences = differing
        .iter()
        .take(3)
        .filter_map(|j| {
            let file = file_lines.get(start + j)?;
            let number = start + j + 1;
            let mut text = format!(
                "line {number}: you wrote `{}`, the file has `{}`",
                clip(needle[*j]),
                clip(file)
            );
            if let Some(reason) = why_differs(needle[*j], file) {
                text.push_str(&format!(" ({reason})"));
            }
            Some(text)
        })
        .collect();

    Some(Candidate {
        first_line: i64::try_from(show_start + 1).unwrap_or(i64::MAX),
        similarity_percent: (score * 100.0).round() as i64,
        shown,
        differences,
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn hint(text: &str, old: &str, new: &str) -> MissHint {
        diagnose(text, &LineIndex::new(text), old, new)
    }

    #[test]
    fn finds_the_closest_lines_and_the_difference() {
        let text = "fn a() {}\nfn compute(x: i32) -> i32 {\n    x + 1\n}\nfn z() {}\n";
        let miss = hint(text, "fn compute(x: i64) -> i32 {\n    x + 1\n}", "");
        let candidate = miss.candidate.expect("candidate");
        assert_eq!(candidate.first_line, 2);
        assert!(candidate.shown[0].contains("compute"), "{candidate:?}");
        assert!(candidate.differences[0].contains("i64"), "{candidate:?}");
        assert!(candidate.similarity_percent > 80);
    }

    #[test]
    fn reports_an_edit_that_was_already_applied() {
        let miss = hint("let y = 2;\n", "let x = 1;", "let y = 2;");
        assert!(miss.already_applied);
    }

    #[test]
    fn no_candidate_for_unrelated_text() {
        let miss = hint("alpha\nbeta\n", "completely different words here", "");
        assert_eq!(miss.candidate, None);
        assert!(miss.to_string().contains("No similar lines"));
    }
}
