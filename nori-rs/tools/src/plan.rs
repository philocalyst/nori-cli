//! Planning a file change without performing it.
//!
//! Every mutating tool is split into a plan (read the file, check the ledger, run the engine,
//! compute the diff) and a commit (the atomic write plus the ledger update). A real call does
//! both; [`Workspace::preview`](crate::Workspace::preview) stops after the plan, so a permission
//! dialog shows exactly what the call would do.

use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use crate::edit::EditReport;
use crate::edit::count_changes;
use crate::edit::unified_diff;
use crate::error::ToolError;
use crate::result::ChangeKind;
use crate::result::EditSummary;
use crate::result::FileChange;
use crate::result::ToolResult;
use crate::util::cpu;
use crate::util::display_path;
use crate::util::line_count;
use crate::workspace::Workspace;

const OPEN: i64 = 0;
const COMMITTING: i64 = 1;
const CANCELLED: i64 = 2;

/// Decides the race between a cancellation and the point of no return of a call.
///
/// A call is cancellable until it starts writing. Once the first write begins, the call runs to
/// completion (the writes are atomic and the ledger is updated), and a late cancellation is
/// ignored; a cancellation that wins the race stops the call before it touches any file.
#[derive(Debug, Default)]
pub(crate) struct CommitGate(AtomicI64);

impl CommitGate {
    /// Called before the first write. Re-entrant once committing.
    pub(crate) fn begin_commit(&self) -> Result<(), ToolError> {
        match self
            .0
            .compare_exchange(OPEN, COMMITTING, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => Ok(()),
            Err(COMMITTING) => Ok(()),
            Err(_) => Err(ToolError::Cancelled),
        }
    }

    /// Called on cancellation: true if the call had not started writing and now never will.
    pub(crate) fn cancel(&self) -> bool {
        self.0
            .compare_exchange(OPEN, CANCELLED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            || self.0.load(Ordering::SeqCst) == CANCELLED
    }
}

/// One file's planned change.
#[derive(Debug)]
pub(crate) struct FilePlan {
    pub path: PathBuf,
    /// `None` for a file that does not exist yet.
    pub old_text: Option<String>,
    pub new_text: String,
    pub diff: String,
    pub added: i64,
    pub removed: i64,
    pub reports: Vec<EditReport>,
    pub notes: Vec<String>,
}

impl FilePlan {
    pub(crate) fn changed(&self) -> bool {
        self.old_text.as_deref() != Some(self.new_text.as_str())
    }

    pub(crate) fn into_change(self) -> FileChange {
        FileChange {
            kind: if self.old_text.is_none() {
                ChangeKind::Created
            } else {
                ChangeKind::Modified
            },
            path: self.path,
            old_text: self.old_text,
            new_text: Some(self.new_text),
            unified_diff: self.diff,
            lines_added: self.added,
            lines_removed: self.removed,
        }
    }
}

impl Workspace {
    /// Build a plan, computing the diff when the text changes.
    pub(crate) async fn make_plan(
        &self,
        path: &Path,
        old_text: Option<String>,
        new_text: String,
        reports: Vec<EditReport>,
        notes: Vec<String>,
    ) -> Result<FilePlan, ToolError> {
        let unchanged = old_text.as_deref() == Some(new_text.as_str());
        let label = display_path(self.cwd(), path);
        let (old, new) = (old_text.clone().unwrap_or_default(), new_text.clone());
        let (diff, (added, removed)) = if unchanged {
            (String::new(), (0, 0))
        } else {
            cpu(old.len() + new.len(), move || {
                (unified_diff(&label, &old, &new), count_changes(&old, &new))
            })
            .await?
        };
        Ok(FilePlan {
            path: path.to_path_buf(),
            old_text,
            new_text,
            diff,
            added,
            removed,
            reports,
            notes,
        })
    }

    /// A plan that creates `path` with `content`.
    pub(crate) async fn plan_create(
        &self,
        path: &Path,
        content: &str,
    ) -> Result<FilePlan, ToolError> {
        self.make_plan(path, None, content.to_owned(), Vec::new(), Vec::new())
            .await
    }

    /// Write the plan's file (the point of no return) and record it in the ledger.
    pub(crate) async fn commit_file(
        &self,
        plan: &FilePlan,
        gate: &CommitGate,
    ) -> Result<(), ToolError> {
        gate.begin_commit()?;
        self.write_text(&plan.path, &plan.new_text).await
    }

    /// The result of a committed creation.
    pub(crate) fn created_result(&self, plan: FilePlan) -> ToolResult {
        let shown = display_path(self.cwd(), &plan.path);
        let lines = line_count(&plan.new_text);
        let mut result = ToolResult::ok(format!("Created {shown} ({lines} lines)."));
        result.summary = Some(EditSummary {
            lines_added: lines,
            lines_removed: 0,
            replacements: 0,
            tier: None,
            changed: true,
            notes: Vec::new(),
        });
        result.changes.push(plan.into_change());
        result
    }
}
