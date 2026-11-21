//! Applying edits to a [`Document`].

use super::document::Document;
use super::error::EditError;
use super::error::Site;
use super::hint::diagnose;
use super::lines::LineIndex;
use super::matcher::MatchTier;
use super::matcher::find;
use super::matcher::plan_indentation;

/// One find-and-replace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub old_string: String,
    pub new_string: String,
    pub replace_all: bool,
}

/// What one edit did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditReport {
    pub tier: MatchTier,
    pub replacements: i64,
    /// 1-based line of each replacement, in the file as it was before this edit.
    pub lines: Vec<i64>,
    /// Plain-language notes, such as how the match was forgiving.
    pub notes: Vec<String>,
}

pub fn to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// If every non-blank line of `text` starts with a `read` line-number prefix (`   12→`), the text
/// with those prefixes removed.
pub fn strip_line_numbers(text: &str) -> Option<String> {
    let mut any = false;
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.trim().is_empty() {
            out.push(line);
            continue;
        }
        let rest = line.trim_start_matches(' ');
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let stripped = rest[digits..].strip_prefix('→')?;
        if digits == 0 {
            return None;
        }
        any = true;
        out.push(stripped);
    }
    any.then(|| out.join("\n"))
}

fn clip(line: &str) -> String {
    let trimmed = line.trim();
    if trimmed.chars().count() > 100 {
        let head: String = trimmed.chars().take(100).collect();
        format!("{head}…")
    } else {
        trimmed.to_owned()
    }
}

impl Document {
    pub fn apply(&mut self, edit: &Edit) -> Result<EditReport, EditError> {
        let mut notes = Vec::new();
        let (old, new) = match strip_line_numbers(&edit.old_string) {
            Some(old) => {
                notes.push(
                    "removed the `read` line-number prefixes (`  12→`) from old_string; they \
                     are not part of the file"
                        .to_owned(),
                );
                let new =
                    strip_line_numbers(&edit.new_string).unwrap_or_else(|| edit.new_string.clone());
                (to_lf(&old), to_lf(&new))
            }
            None => (to_lf(&edit.old_string), to_lf(&edit.new_string)),
        };

        if old.is_empty() {
            if self.lf().is_empty() && !new.is_empty() {
                self.set_all(&new);
                return Ok(EditReport {
                    tier: MatchTier::Exact,
                    replacements: 1,
                    lines: vec![1],
                    notes,
                });
            }
            return Err(EditError::EmptyOldString);
        }
        if old == new {
            return Err(EditError::Identical);
        }

        let index = LineIndex::new(self.lf());
        let Some((mut tier, found)) = find(self.lf(), &index, &old) else {
            return Err(EditError::NoMatch(diagnose(self.lf(), &index, &old, &new)));
        };
        if found.len() > 1 && !edit.replace_all {
            let sites = found
                .iter()
                .map(|found| {
                    let line = index.line_of(found.start);
                    Site {
                        line: i64::try_from(line + 1).unwrap_or(i64::MAX),
                        text: clip(index.text(self.lf(), line)),
                    }
                })
                .collect();
            return Err(EditError::Ambiguous { tier, sites });
        }
        if tier == MatchTier::Exact && old.contains('\n') && self.has_crlf() {
            tier = MatchTier::LineEndings;
        }

        let mut replacements = Vec::with_capacity(found.len());
        let mut lines = Vec::with_capacity(found.len());
        let mut indent_notes: Vec<String> = Vec::new();
        for found in &found {
            lines.push(i64::try_from(index.line_of(found.start) + 1).unwrap_or(i64::MAX));
            let text = if tier == MatchTier::Indentation {
                let plan = plan_indentation(self.lf(), &index, &old, found, &new);
                if let Some(description) = plan.describe()
                    && !indent_notes.contains(&description)
                {
                    indent_notes.push(description);
                }
                plan.apply(&new)
            } else {
                new.clone()
            };
            replacements.push((found.start, found.end, text));
        }
        if tier != MatchTier::Exact {
            let mut note = tier.describe().to_owned();
            if !indent_notes.is_empty() {
                note.push_str("; ");
                note.push_str(&indent_notes.join("; "));
            }
            notes.push(note);
        }

        self.replace_ranges(&replacements);
        Ok(EditReport {
            tier,
            replacements: i64::try_from(replacements.len()).unwrap_or(i64::MAX),
            lines,
            notes,
        })
    }

    /// Apply `edits` in order, each seeing the result of the earlier ones. All or nothing: on
    /// failure the document is left as it was.
    pub fn apply_all(&mut self, edits: &[Edit]) -> Result<Vec<EditReport>, EditError> {
        if edits.is_empty() {
            return Err(EditError::NoEdits);
        }
        let mut working = self.clone();
        let mut reports = Vec::with_capacity(edits.len());
        for (position, edit) in edits.iter().enumerate() {
            let report = working.apply(edit).map_err(|source| {
                if edits.len() == 1 {
                    source
                } else {
                    EditError::InMulti {
                        index: i64::try_from(position + 1).unwrap_or(i64::MAX),
                        total: i64::try_from(edits.len()).unwrap_or(i64::MAX),
                        source: Box::new(source),
                    }
                }
            })?;
            reports.push(report);
        }
        *self = working;
        Ok(reports)
    }
}
