//! The workspace the tools run in: a root directory, a filesystem, the read ledger and limits.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use serde_json::Value;
use tokio::sync::OwnedMutexGuard;
use tokio_util::sync::CancellationToken;

use crate::error::ToolError;
use crate::fs::LocalFs;
use crate::fs::ToolFs;
use crate::ledger::ReadLedger;
use crate::plan::CommitGate;
use crate::plan::FilePlan;
use crate::preview::PermissionAction;
use crate::preview::Preview;
use crate::preview::PreviewChange;
use crate::preview::PreviewPath;
use crate::repair::ToolCall;
use crate::repair::parse_call;
use crate::result::ChangeKind;
use crate::result::ToolResult;
use crate::util::normalize;

/// Behaviour switches and limits. The defaults are right for an interactive coding agent.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    /// Refuse `edit`, `multi_edit` and `write` on an existing file the model has not read (or
    /// that changed since). On by default.
    pub require_read_before_edit: bool,
    /// Also require a fresh read before `*** Update File` hunks. Off by default: a patch's own
    /// context lines already prove the model knows the file.
    pub patch_requires_read: bool,
    /// Largest text file the tools will load.
    pub max_file_bytes: i64,
    /// Lines returned by one `read` when no limit is given, and the most it will return.
    pub read_line_limit: i64,
    /// Characters kept of one line before it is cut.
    pub max_line_chars: i64,
    /// Characters of output one `read` returns before it stops and says where to continue.
    pub max_read_output_chars: i64,
    /// Largest image a `read` attaches.
    pub max_image_bytes: i64,
    /// Directories besides the working directory that count as inside the workspace. A path
    /// under none of them (and not under the working directory) is an external directory; see
    /// [`Workspace::is_external`]. The tools never refuse an external path themselves.
    pub roots: Vec<PathBuf>,
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            require_read_before_edit: true,
            patch_requires_read: false,
            max_file_bytes: 32 * 1024 * 1024,
            read_line_limit: 2000,
            max_line_chars: 2000,
            max_read_output_chars: 60_000,
            max_image_bytes: 8 * 1024 * 1024,
            roots: Vec::new(),
        }
    }
}

struct Inner {
    cwd: PathBuf,
    fs: Arc<dyn ToolFs>,
    ledger: ReadLedger,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    config: WorkspaceConfig,
}

/// One session's file tools. Cheap to clone; clones share the ledger and the path locks.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Workspace")
            .field("cwd", &self.inner.cwd)
            .field("fs", &self.inner.fs)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// The local machine, rooted at `cwd`, with default limits.
    pub fn local(cwd: impl Into<PathBuf>) -> Self {
        Self::new(cwd, Arc::new(LocalFs), WorkspaceConfig::default())
    }

    pub fn new(cwd: impl Into<PathBuf>, fs: Arc<dyn ToolFs>, mut config: WorkspaceConfig) -> Self {
        let cwd = normalize(&cwd.into());
        config.roots = config
            .roots
            .iter()
            .map(|root| {
                if root.is_absolute() {
                    normalize(root)
                } else {
                    normalize(&cwd.join(root))
                }
            })
            .collect();
        Self {
            inner: Arc::new(Inner {
                cwd,
                fs,
                ledger: ReadLedger::new(),
                locks: Mutex::new(HashMap::new()),
                config,
            }),
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.inner.cwd
    }

    pub fn ledger(&self) -> &ReadLedger {
        &self.inner.ledger
    }

    pub fn fs(&self) -> &dyn ToolFs {
        self.inner.fs.as_ref()
    }

    pub fn config(&self) -> &WorkspaceConfig {
        &self.inner.config
    }

    /// A model-supplied path made absolute against the workspace root and normalised.
    pub fn resolve_path(&self, raw: &str) -> PathBuf {
        let raw = raw.trim();
        let path = Path::new(raw);
        if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.inner.cwd.join(path))
        }
    }

    /// Serialise read-modify-write cycles on one path (parallel tool calls on the same file).
    pub(crate) async fn lock_path(&self, path: &Path) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self
                .inner
                .locks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            Arc::clone(locks.entry(path.to_path_buf()).or_default())
        };
        lock.lock_owned().await
    }

    /// Whether `path` lies outside the working directory and every extra root. Purely lexical
    /// (`..` is resolved, symlinks are not); a relative path is taken against the working
    /// directory.
    pub fn is_external(&self, path: &Path) -> bool {
        let path = if path.is_absolute() {
            normalize(path)
        } else {
            normalize(&self.inner.cwd.join(path))
        };
        !std::iter::once(&self.inner.cwd)
            .chain(self.inner.config.roots.iter())
            .any(|root| path.starts_with(root))
    }

    /// Run a tool by name (or alias) with JSON arguments. Never panics and never fails as a
    /// call: an error comes back as a result with `is_error` set and text saying how to recover.
    pub async fn call(&self, name: &str, args: Value) -> ToolResult {
        self.call_with_cancel(name, args, CancellationToken::new())
            .await
    }

    /// [`call`](Self::call) that stops promptly when `cancel` fires.
    ///
    /// A call cancelled before it starts writing returns an error result, "cancelled by the
    /// user", and has not touched any file. Writes are atomic (temporary file and rename) and the
    /// call is uncancellable from its first write to its last: if the token fires during that
    /// window the call finishes and returns its real result, so a file is never left half
    /// written or out of step with the ledger.
    pub async fn call_with_cancel(
        &self,
        name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> ToolResult {
        if cancel.is_cancelled() {
            return ToolResult::from_error(&ToolError::Cancelled);
        }
        let gate = CommitGate::default();
        let work = self.dispatch(name, args, &gate);
        tokio::pin!(work);
        let outcome = tokio::select! {
            biased;
            outcome = &mut work => outcome,
            () = cancel.cancelled() => {
                if gate.cancel() {
                    return ToolResult::from_error(&ToolError::Cancelled);
                }
                work.await
            }
        };
        match outcome {
            Ok(result) => result,
            Err(error) => ToolResult::from_error(&error),
        }
    }

    /// What a call would do, without doing it: the paths it touches (and whether each is outside
    /// the workspace) and, for a change, the exact old and new text. The call is repaired,
    /// matched and checked against the read ledger exactly as the real call would be, so a call
    /// that would fail fails here with the same error. Nothing is written and the ledger is not
    /// touched.
    pub async fn preview(&self, name: &str, args: Value) -> Result<Preview, ToolError> {
        match parse_call(name, args)? {
            ToolCall::Read(args) => {
                let path = self.resolve_path(&args.file_path);
                self.stat_existing(&path).await?;
                Ok(self.make_preview(PermissionAction::Read, vec![path], Vec::new()))
            }
            ToolCall::Write(args) => {
                let path = self.resolve_path(&args.file_path);
                let plan = self.plan_write(&path, &args.content).await?;
                Ok(self.preview_of_plan(plan))
            }
            ToolCall::Edit(args) => {
                let (file_path, edits) = args.into_edits();
                let path = self.resolve_path(&file_path);
                let plan = self.plan_edit(&path, &edits).await?;
                Ok(self.preview_of_plan(plan))
            }
            ToolCall::MultiEdit(args) => {
                let (file_path, edits) = args.into_edits();
                let path = self.resolve_path(&file_path);
                let plan = self.plan_edit(&path, &edits).await?;
                Ok(self.preview_of_plan(plan))
            }
            ToolCall::ApplyPatch(patch) => {
                let hunks = crate::patch::parse_patch(&patch)?;
                let paths = crate::patch::touched_paths(self, &hunks);
                let state = crate::patch::plan_patch(self, &hunks).await?;
                let changes = crate::patch::changed_entries(state)
                    .into_iter()
                    .map(|(path, old_text, new_text)| PreviewChange {
                        kind: match (&old_text, &new_text) {
                            (None, _) => ChangeKind::Created,
                            (Some(_), None) => ChangeKind::Deleted,
                            (Some(_), Some(_)) => ChangeKind::Modified,
                        },
                        path,
                        old_text,
                        new_text: new_text.unwrap_or_default(),
                    })
                    .collect();
                Ok(self.make_preview(PermissionAction::Edit, paths, changes))
            }
        }
    }

    fn preview_of_plan(&self, plan: FilePlan) -> Preview {
        let changes = if plan.changed() {
            vec![PreviewChange {
                kind: if plan.old_text.is_none() {
                    ChangeKind::Created
                } else {
                    ChangeKind::Modified
                },
                path: plan.path.clone(),
                old_text: plan.old_text,
                new_text: plan.new_text,
            }]
        } else {
            Vec::new()
        };
        self.make_preview(PermissionAction::Edit, vec![plan.path], changes)
    }

    fn make_preview(
        &self,
        action: PermissionAction,
        paths: Vec<PathBuf>,
        changes: Vec<PreviewChange>,
    ) -> Preview {
        let paths = paths
            .into_iter()
            .map(|path| PreviewPath {
                external: self.is_external(&path),
                path,
            })
            .collect();
        Preview {
            action,
            paths,
            changes,
        }
    }

    async fn dispatch(
        &self,
        name: &str,
        args: Value,
        gate: &CommitGate,
    ) -> Result<ToolResult, ToolError> {
        match parse_call(name, args)? {
            ToolCall::Read(args) => self.read(args).await,
            ToolCall::Write(args) => self.write(args, gate).await,
            ToolCall::Edit(args) => {
                let (file_path, edits) = args.into_edits();
                self.edit_file(&file_path, edits, gate).await
            }
            ToolCall::MultiEdit(args) => {
                let (file_path, edits) = args.into_edits();
                self.edit_file(&file_path, edits, gate).await
            }
            ToolCall::ApplyPatch(patch) => crate::patch::run(self, &patch, gate).await,
        }
    }
}
