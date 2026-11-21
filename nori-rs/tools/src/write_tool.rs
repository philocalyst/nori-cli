//! The `write` tool: create a file or replace one whole.

use std::path::Path;

use crate::edit::Document;
use crate::edit::to_lf;
use crate::edit_tool::capped_diff;
use crate::error::ToolError;
use crate::fs::FileKind;
use crate::plan::CommitGate;
use crate::plan::FilePlan;
use crate::repair::WriteArgs;
use crate::result::EditSummary;
use crate::result::ToolResult;
use crate::util::cpu;
use crate::util::display_path;
use crate::workspace::Workspace;

impl Workspace {
    /// Everything a write does short of writing.
    pub(crate) async fn plan_write(
        &self,
        path: &Path,
        content: &str,
    ) -> Result<FilePlan, ToolError> {
        let Some(stat) = self.stat_optional(path).await? else {
            return self.plan_create(path, content).await;
        };
        if stat.kind == FileKind::Dir {
            return Err(ToolError::IsDirectory {
                path: path.to_path_buf(),
            });
        }
        let loaded = self.load_text_with(path, &stat).await?;
        self.require_fresh(path, &loaded)?;

        let old_text = loaded.text;
        let size = old_text.len() + content.len();
        let (new_text, kept_endings) = {
            let (old, new) = (old_text.clone(), content.to_owned());
            cpu(size, move || {
                // Content with no carriage returns takes the file's own line endings and BOM.
                if new.contains('\r') {
                    return (new, false);
                }
                let mut document = Document::parse(&old);
                let had_crlf = document.has_crlf();
                document.set_all(&to_lf(&new));
                (document.render(), had_crlf)
            })
            .await?
        };
        let notes = if kept_endings {
            vec!["kept the file's CRLF line endings".to_owned()]
        } else {
            Vec::new()
        };
        self.make_plan(path, Some(old_text), new_text, Vec::new(), notes)
            .await
    }

    pub(crate) async fn write(
        &self,
        args: WriteArgs,
        gate: &CommitGate,
    ) -> Result<ToolResult, ToolError> {
        let path = self.resolve_path(&args.file_path);
        let _guard = self.lock_path(&path).await;
        let plan = self.plan_write(&path, &args.content).await?;
        let shown = display_path(self.cwd(), &path);

        if !plan.changed() {
            let mut result = ToolResult::ok(format!(
                "No changes made to {shown}: it already has exactly this content."
            ));
            result.summary = Some(EditSummary {
                lines_added: 0,
                lines_removed: 0,
                replacements: 0,
                tier: None,
                changed: false,
                notes: Vec::new(),
            });
            return Ok(result);
        }
        self.commit_file(&plan, gate).await?;
        if plan.old_text.is_none() {
            return Ok(self.created_result(plan));
        }

        let mut text = format!("Wrote {shown}: +{} -{} lines.", plan.added, plan.removed);
        if !plan.notes.is_empty() {
            text.push_str(&format!(" ({})", plan.notes.join("; ")));
        }
        text.push_str("\n\n");
        text.push_str(&capped_diff(&plan.diff));
        let mut result = ToolResult::ok(text);
        result.summary = Some(EditSummary {
            lines_added: plan.added,
            lines_removed: plan.removed,
            replacements: 0,
            tier: None,
            changed: true,
            notes: plan.notes.clone(),
        });
        result.changes.push(plan.into_change());
        Ok(result)
    }
}
