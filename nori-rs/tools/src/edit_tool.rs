//! The `edit` and `multi_edit` tools: the engine plus the ledger, locking and file I/O.

use std::path::Path;

use crate::edit::Document;
use crate::edit::Edit;
use crate::edit::EditError;
use crate::edit::EditReport;
use crate::edit::MatchTier;
use crate::error::ToolError;
use crate::fs::FileKind;
use crate::plan::CommitGate;
use crate::plan::FilePlan;
use crate::repair::EditArgs;
use crate::repair::MultiEditArgs;
use crate::result::EditSummary;
use crate::result::ToolResult;
use crate::util::cpu;
use crate::util::display_path;
use crate::workspace::Workspace;

/// Lines of diff shown to the model; the full diff is in the structured result.
const MODEL_DIFF_LINES: usize = 60;

struct Engine {
    outcome: Result<Vec<EditReport>, EditError>,
    old_text: String,
    new_text: String,
}

/// A diff for the model, cut to a sensible size.
pub(crate) fn capped_diff(diff: &str) -> String {
    let lines: Vec<&str> = diff.lines().collect();
    if lines.len() <= MODEL_DIFF_LINES {
        return diff.trim_end().to_owned();
    }
    let mut out = lines[..MODEL_DIFF_LINES].join("\n");
    out.push_str(&format!(
        "\n… diff truncated ({} more lines)",
        lines.len() - MODEL_DIFF_LINES
    ));
    out
}

impl EditArgs {
    pub(crate) fn into_edits(self) -> (String, Vec<Edit>) {
        let edit = Edit {
            old_string: self.old_string,
            new_string: self.new_string,
            replace_all: self.replace_all,
        };
        (self.file_path, vec![edit])
    }
}

impl MultiEditArgs {
    pub(crate) fn into_edits(self) -> (String, Vec<Edit>) {
        let edits = self
            .edits
            .into_iter()
            .map(|item| Edit {
                old_string: item.old_string,
                new_string: item.new_string,
                replace_all: item.replace_all,
            })
            .collect();
        (self.file_path, edits)
    }
}

impl Workspace {
    /// Everything an edit does short of writing: the ledger check, the engine and the diff.
    pub(crate) async fn plan_edit(
        &self,
        path: &Path,
        edits: &[Edit],
    ) -> Result<FilePlan, ToolError> {
        let Some(stat) = self.stat_optional(path).await? else {
            return match edits {
                [only] if only.old_string.is_empty() && !only.new_string.is_empty() => {
                    self.plan_create(path, &only.new_string).await
                }
                [_] | [] | [_, ..] => Err(ToolError::NotFound {
                    path: path.to_path_buf(),
                    suggestions: crate::suggest::suggest(self.fs(), path).await,
                }),
            };
        };
        if stat.kind == FileKind::Dir {
            return Err(ToolError::IsDirectory {
                path: path.to_path_buf(),
            });
        }
        let loaded = self.load_text_with(path, &stat).await?;
        self.require_fresh(path, &loaded)?;

        let engine_edits = edits.to_vec();
        let text = loaded.text;
        let engine = cpu(text.len(), move || {
            let mut document = Document::parse(&text);
            let outcome = document.apply_all(&engine_edits);
            let new_text = document.render();
            Engine {
                outcome,
                old_text: text,
                new_text,
            }
        })
        .await?;
        let reports = engine.outcome.map_err(|source| ToolError::Edit {
            path: path.to_path_buf(),
            source,
        })?;
        let notes = summarize(&reports, 0, 0, false).notes;
        self.make_plan(path, Some(engine.old_text), engine.new_text, reports, notes)
            .await
    }

    pub(crate) async fn edit_file(
        &self,
        file_path: &str,
        edits: Vec<Edit>,
        gate: &CommitGate,
    ) -> Result<ToolResult, ToolError> {
        let path = self.resolve_path(file_path);
        let _guard = self.lock_path(&path).await;
        let plan = self.plan_edit(&path, &edits).await?;
        let shown = display_path(self.cwd(), &path);

        if !plan.changed() {
            let mut result = ToolResult::ok(format!(
                "No changes made to {shown}: the file already contains the result of this edit."
            ));
            result.summary = Some(summarize(&plan.reports, 0, 0, false));
            return Ok(result);
        }
        self.commit_file(&plan, gate).await?;
        if plan.old_text.is_none() {
            return Ok(self.created_result(plan));
        }

        let summary = summarize(&plan.reports, plan.added, plan.removed, true);
        let mut text = format!(
            "Edited {shown}: {} replacement{}, +{} -{} lines.",
            summary.replacements,
            if summary.replacements == 1 { "" } else { "s" },
            plan.added,
            plan.removed,
        );
        if !summary.notes.is_empty() {
            text.push_str("\nNote: ");
            text.push_str(&summary.notes.join(" | "));
            text.push_str(". Check the diff to confirm it is what you intended.");
        }
        text.push_str("\n\n");
        text.push_str(&capped_diff(&plan.diff));

        let mut result = ToolResult::ok(text);
        result.summary = Some(summary);
        result.changes.push(plan.into_change());
        Ok(result)
    }
}

fn summarize(reports: &[EditReport], added: i64, removed: i64, changed: bool) -> EditSummary {
    let tier: Option<MatchTier> = reports.iter().map(|report| report.tier).max();
    let mut notes: Vec<String> = Vec::new();
    for note in reports.iter().flat_map(|report| report.notes.iter()) {
        if !notes.contains(note) {
            notes.push(note.clone());
        }
    }
    EditSummary {
        lines_added: added,
        lines_removed: removed,
        replacements: reports.iter().map(|report| report.replacements).sum(),
        tier,
        changed,
        notes,
    }
}
