//! Running a parsed patch against the workspace: validate everything in memory, then commit.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use super::Hunk;
use super::PatchError;
use super::chunks::apply_chunks;
use super::parse_patch;
use crate::edit::count_changes;
use crate::edit::unified_diff;
use crate::edit_tool::capped_diff;
use crate::error::ToolError;
use crate::fs::FileKind;
use crate::io::LoadedFile;
use crate::plan::CommitGate;
use crate::result::ChangeKind;
use crate::result::EditSummary;
use crate::result::FileChange;
use crate::result::ToolResult;
use crate::suggest::suggest;
use crate::util::cpu;
use crate::util::display_path;
use crate::workspace::Workspace;

/// One file's state while the patch is being validated.
pub(crate) struct Entry {
    loaded: Option<LoadedFile>,
    current: Option<String>,
}

pub(crate) type State = BTreeMap<PathBuf, Entry>;

async fn ensure(ws: &Workspace, state: &mut State, path: &Path) -> Result<(), ToolError> {
    if state.contains_key(path) {
        return Ok(());
    }
    let entry = match ws.stat_optional(path).await? {
        None => Entry {
            loaded: None,
            current: None,
        },
        Some(stat) if stat.kind == FileKind::Dir => {
            return Err(ToolError::IsDirectory {
                path: path.to_path_buf(),
            });
        }
        Some(stat) => {
            let loaded = ws.load_text_with(path, &stat).await?;
            Entry {
                current: Some(loaded.text.clone()),
                loaded: Some(loaded),
            }
        }
    };
    state.insert(path.to_path_buf(), entry);
    Ok(())
}

fn current<'a>(state: &'a State, path: &Path) -> Option<&'a String> {
    state.get(path).and_then(|entry| entry.current.as_ref())
}

fn set(state: &mut State, path: &Path, value: Option<String>) {
    if let Some(entry) = state.get_mut(path) {
        entry.current = value;
    }
}

/// Every path a patch touches, resolved and sorted.
pub(crate) fn touched_paths(ws: &Workspace, hunks: &[Hunk]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::Add { path, .. } | Hunk::Delete { path } => paths.push(ws.resolve_path(path)),
            Hunk::Update { path, move_to, .. } => {
                paths.push(ws.resolve_path(path));
                if let Some(target) = move_to {
                    paths.push(ws.resolve_path(target));
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

/// What a patch would leave each changed file as: `(path, before, after)`, `None` meaning absent.
pub(crate) type PatchChange = (PathBuf, Option<String>, Option<String>);

pub(crate) fn changed_entries(state: State) -> Vec<PatchChange> {
    state
        .into_iter()
        .filter_map(|(path, entry)| {
            let original = entry.loaded.map(|loaded| loaded.text);
            (entry.current != original).then_some((path, original, entry.current))
        })
        .collect()
}

/// Validate every hunk in memory; nothing is written.
pub(crate) async fn plan(ws: &Workspace, hunks: &[Hunk]) -> Result<State, ToolError> {
    let mut state = State::new();
    for hunk in hunks {
        match hunk {
            Hunk::Add { path, contents } => {
                let resolved = ws.resolve_path(path);
                ensure(ws, &mut state, &resolved).await?;
                match current(&state, &resolved) {
                    Some(existing) if existing == contents => continue,
                    Some(_) => return Err(ToolError::AlreadyExists { path: resolved }),
                    None => set(&mut state, &resolved, Some(contents.clone())),
                }
            }
            Hunk::Delete { path } => {
                let resolved = ws.resolve_path(path);
                ensure(ws, &mut state, &resolved).await?;
                if current(&state, &resolved).is_none() {
                    return Err(PatchError::DeleteMissing { path: path.clone() }.into());
                }
                set(&mut state, &resolved, None);
            }
            Hunk::Update {
                path,
                move_to,
                chunks,
            } => {
                let resolved = ws.resolve_path(path);
                ensure(ws, &mut state, &resolved).await?;
                let Some(text) = current(&state, &resolved).cloned() else {
                    return Err(ToolError::NotFound {
                        suggestions: suggest(ws.fs(), &resolved).await,
                        path: resolved,
                    });
                };
                if ws.config().patch_requires_read
                    && let Some(loaded) =
                        state.get(&resolved).and_then(|entry| entry.loaded.as_ref())
                {
                    ws.require_fresh(&resolved, loaded)?;
                }
                let updated = if chunks.is_empty() {
                    text
                } else {
                    apply_chunks(path, &text, chunks)?
                };
                match move_to {
                    Some(target) => {
                        let target_path = ws.resolve_path(target);
                        if target_path != resolved {
                            ensure(ws, &mut state, &target_path).await?;
                            if current(&state, &target_path).is_some() {
                                return Err(PatchError::MoveTargetExists {
                                    path: path.clone(),
                                    target: target.clone(),
                                }
                                .into());
                            }
                            set(&mut state, &target_path, Some(updated));
                            set(&mut state, &resolved, None);
                        } else {
                            set(&mut state, &resolved, Some(updated));
                        }
                    }
                    None => set(&mut state, &resolved, Some(updated)),
                }
            }
        }
    }
    Ok(state)
}

pub(crate) async fn run(
    ws: &Workspace,
    patch: &str,
    gate: &CommitGate,
) -> Result<ToolResult, ToolError> {
    let hunks = parse_patch(patch)?;
    let paths = touched_paths(ws, &hunks);
    let mut guards = Vec::with_capacity(paths.len());
    for path in &paths {
        guards.push(ws.lock_path(path).await);
    }
    let state = plan(ws, &hunks).await?;
    commit(ws, state, gate).await
}

async fn commit(ws: &Workspace, state: State, gate: &CommitGate) -> Result<ToolResult, ToolError> {
    let changed = changed_entries(state);
    if !changed.is_empty() {
        gate.begin_commit()?;
    }
    let mut applied: Vec<(PathBuf, Option<String>)> = Vec::new();
    let mut changes = Vec::new();
    let mut listing = Vec::new();
    let mut diffs = Vec::new();

    for (path, original, current) in changed {
        let outcome = match &current {
            Some(text) => ws.write_text(&path, text).await,
            None => {
                let removed = ws
                    .fs()
                    .remove_file(&path)
                    .await
                    .map_err(|error| ToolError::io("delete", &path, error));
                if removed.is_ok() {
                    ws.ledger().forget(&path);
                }
                removed
            }
        };
        if let Err(error) = outcome {
            rollback(ws, &applied).await;
            return Err(error);
        }
        applied.push((path.clone(), original.clone()));

        let shown = display_path(ws.cwd(), &path);
        let (kind, marker) = match (&original, &current) {
            (None, _) => (ChangeKind::Created, "A"),
            (Some(_), None) => (ChangeKind::Deleted, "D"),
            (Some(_), Some(_)) => (ChangeKind::Modified, "M"),
        };
        listing.push(format!("{marker} {shown}"));
        let (old, new) = (
            original.clone().unwrap_or_default(),
            current.clone().unwrap_or_default(),
        );
        let label = shown;
        let (diff, (added, removed)) = cpu(old.len() + new.len(), move || {
            (unified_diff(&label, &old, &new), count_changes(&old, &new))
        })
        .await?;
        diffs.push(diff.clone());
        changes.push(FileChange {
            path,
            kind,
            old_text: original,
            new_text: current,
            unified_diff: diff,
            lines_added: added,
            lines_removed: removed,
        });
    }

    if changes.is_empty() {
        return Ok(ToolResult::ok(
            "No changes made: every file already has the content this patch produces.",
        ));
    }
    let added = changes.iter().map(|change| change.lines_added).sum();
    let removed = changes.iter().map(|change| change.lines_removed).sum();
    let text = format!(
        "Patch applied. Files changed:\n{}\n\n{}",
        listing.join("\n"),
        capped_diff(&diffs.join("\n"))
    );
    let mut result = ToolResult::ok(text);
    result.summary = Some(EditSummary {
        lines_added: added,
        lines_removed: removed,
        replacements: i64::try_from(changes.len()).unwrap_or(i64::MAX),
        tier: None,
        changed: true,
        notes: Vec::new(),
    });
    result.changes = changes;
    Ok(result)
}

/// Best-effort undo of the files already written when a later one fails.
async fn rollback(ws: &Workspace, applied: &[(PathBuf, Option<String>)]) {
    for (path, original) in applied.iter().rev() {
        match original {
            Some(text) => {
                let _ = ws.write_text(path, text).await;
            }
            None => {
                let _ = ws.fs().remove_file(path).await;
                ws.ledger().forget(path);
            }
        }
    }
}
