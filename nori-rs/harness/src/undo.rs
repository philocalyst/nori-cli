//! Undo support for the ACP backend through an edit journal.
//!
//! ACP hands the harness every file edit the agent reports as a diff. The
//! [`EditJournal`] records, per turn, what each reported edit replaced, and
//! [`handle_undo`] / [`handle_undo_to`] write that content back. Nothing runs on
//! the prompt-submit path: turns are opened and edits are recorded on the session
//! runtime task, and restoring (the only thing that reads the disk or asks git)
//! runs only when the user asks for `/undo`.
//!
//! Edits the agent makes without reporting a diff (for example through a shell command) are not in
//! that journal. Where the working directory is inside a git work tree, every turn therefore also
//! records the project's tree before its prompt is sent (see [`tree`] and [`turn_tree`]), and undo
//! restores from that tree first, so a turn that ran `sed` or `rm` is reverted like one that used
//! the agent's editor. Without a snapshot (no git, a tree too big to capture, a capture that
//! failed) the journal alone applies, and an undo says so when the turn ran commands.
//!
//! What an edit replaced comes from the agent's report alone, never from reading the file when the
//! report arrives (the write may already have landed): a diff with `old_text` replaced that text; a
//! diff without it creates a new file (the ACP meaning), unless the journal already knows the path
//! existed earlier in the session, in which case the earlier known content stands as what this edit
//! replaced, or git's HEAD tracks the path (asked when the undo runs), in which case the file was
//! not new and HEAD's text is what it held.
//!
//! Undo is non-destructive by construction: whatever it would delete or overwrite is first copied to
//! `$NORI_HOME/undo-trash` (see [`trash`]), and the result says where.

mod changed_view;
mod git_head;
mod trash;
mod tree;
mod tree_undo;
mod turn_tree;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;

use git_head::HeadState;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::UndoEvent;
use tokio::sync::mpsc;
use tracing::info;
use tracing::warn;

use crate::backend::BackendEvent;
use crate::normalized::Artifact;
use crate::normalized::FileOperation;
use crate::normalized::Invocation;
use crate::normalized::ToolKind;
use crate::normalized::ToolSnapshot;
use trash::UndoTrash;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoSnapshot {
    pub index: i64,
    /// Short tag shown next to the label, e.g. `2 files`.
    pub short_id: String,
    pub label: String,
}

/// What a file held before the agent's edit.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    /// The file held this content (`None`: it did not exist).
    Known(Option<String>),
    /// The report gave no previous content, which in ACP means the edit created the file. Undo
    /// checks git's HEAD before believing it.
    Created,
    /// The journal cannot tell what the file held, so the edit cannot be
    /// reversed safely.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEdit {
    path: PathBuf,
    before: Origin,
    /// Content after the edit (`None`: the file was deleted). For diffs that
    /// carry only the changed region this is that region.
    after: Option<String>,
}

#[derive(Debug, Default)]
struct CallEdits {
    edits: Vec<FileEdit>,
    /// Position in `edits` of each path, so a later snapshot of the same call finds its edit
    /// without scanning.
    edit_index: HashMap<PathBuf, usize>,
}

#[derive(Debug)]
struct TurnEdits {
    /// Identifies the turn across the background reads that finish after it was recorded.
    id: u64,
    label: String,
    calls: Vec<CallEdits>,
    /// Position in `calls` of each tool call, so a later snapshot of the same call finds it
    /// without scanning.
    call_index: HashMap<String, usize>,
    ran_commands: bool,
    trees: turn_tree::TurnTrees,
}

impl TurnEdits {
    fn touched_paths(&self) -> usize {
        let mut paths: Vec<&Path> = self
            .calls
            .iter()
            .flat_map(|call| call.edits.iter().map(|edit| edit.path.as_path()))
            .collect();
        paths.sort_unstable();
        paths.dedup();
        paths.len()
    }

    fn is_undoable(&self) -> bool {
        match self.trees.changed() {
            // The snapshot saw nothing change; only an edit the agent reported can still matter
            // (to a file the snapshot ignores).
            Some(changed) => changed || !self.calls.is_empty(),
            None => !self.calls.is_empty() || self.ran_commands,
        }
    }
}

/// Error returned by [`EditJournal::take_through_index`] when the requested
/// index cannot be fulfilled.
pub enum RestoreError {
    /// The journal holds no undoable turn.
    Empty,
    /// The index is out of bounds for the number of undoable turns.
    OutOfBounds,
}

/// Outcome of writing one turn's recorded content back to disk.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored: Vec<PathBuf>,
    pub skipped: Vec<(PathBuf, String)>,
    /// Files undo deleted or overwrote, and where their content was kept first.
    pub kept: Vec<(PathBuf, PathBuf)>,
    /// The paths in `restored` came from the turn snapshot, not from the edit journal.
    pub from_snapshot: bool,
    pub ran_commands: bool,
}

/// Per-turn record of the file edits the agent reported.
pub struct EditJournal {
    turns: Mutex<Vec<TurnEdits>>,
    next_turn_id: std::sync::atomic::AtomicU64,
    trash: UndoTrash,
    /// The project's tree snapshots; `None` for a journal that keeps only reported edits.
    snapshots: Option<turn_tree::TurnSnapshots>,
}

impl Default for EditJournal {
    fn default() -> Self {
        Self::new()
    }
}

impl EditJournal {
    pub fn new() -> Self {
        Self {
            turns: Mutex::new(Vec::new()),
            next_turn_id: std::sync::atomic::AtomicU64::new(0),
            trash: UndoTrash::under_nori_home(&uuid::Uuid::new_v4().to_string()),
            snapshots: None,
        }
    }

    fn with_trash(trash: UndoTrash) -> Self {
        Self {
            trash,
            ..Self::new()
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<TurnEdits>> {
        self.turns
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Open a turn. Edits reported until the next call belong to it.
    pub fn begin_turn(&self, label: String) {
        let id = self
            .next_turn_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.lock().push(TurnEdits {
            id,
            label,
            calls: Vec::new(),
            call_index: HashMap::new(),
            ran_commands: false,
            trees: turn_tree::TurnTrees::default(),
        });
    }

    /// Record the file edits carried by a tool snapshot in the open turn.
    ///
    /// `scope` names the subagent session the snapshot came from, so that call ids of different
    /// sessions cannot collide; a child's edits belong to the current root turn.
    ///
    /// The cost is proportional to the size of the snapshot's diffs and, for a diff that creates a
    /// file, to the number of edits already journaled. The disk is never read. Snapshots arriving
    /// with no open turn (session replay) are ignored.
    pub fn record(&self, snapshot: &ToolSnapshot, cwd: &Path, scope: Option<&str>) {
        let reported = reported_edits(snapshot, cwd);
        let mut turns = self.lock();
        let Some(turn_position) = turns.len().checked_sub(1) else {
            return;
        };
        if snapshot.kind == ToolKind::Execute {
            turns[turn_position].ran_commands = true;
        }
        if reported.is_empty() {
            return;
        }
        let key = match scope {
            Some(scope) => format!("{scope}/{}", snapshot.call_id),
            None => snapshot.call_id.clone(),
        };
        let call_position = match turns[turn_position].call_index.get(&key) {
            Some(position) => *position,
            None => {
                let turn = &mut turns[turn_position];
                turn.calls.push(CallEdits::default());
                turn.call_index.insert(key, turn.calls.len() - 1);
                turn.calls.len() - 1
            }
        };
        for reported in reported {
            // A later snapshot of the same call refines the result; what the file held
            // before the call does not change.
            if let Some(position) = turns[turn_position].calls[call_position]
                .edit_index
                .get(&reported.path)
                .copied()
            {
                turns[turn_position].calls[call_position].edits[position].after = reported.after;
                continue;
            }
            let before = match reported.before {
                Before::Replaced(text) => Origin::Known(Some(text)),
                Before::NewFile => known_before_creation(&turns, &reported.path),
            };
            let call = &mut turns[turn_position].calls[call_position];
            call.edit_index
                .insert(reported.path.clone(), call.edits.len());
            call.edits.push(FileEdit {
                path: reported.path,
                before,
                after: reported.after,
            });
        }
    }

    /// Undoable turns, most recent first.
    pub fn list(&self) -> Vec<UndoSnapshot> {
        self.lock()
            .iter()
            .rev()
            .filter(|turn| turn.is_undoable())
            .enumerate()
            .map(|(index, turn)| {
                let files = match turn.trees.changed_files {
                    Some(changed) if changed > 0 => changed as usize,
                    Some(_) | None => turn.touched_paths(),
                };
                let short_id = match (files, turn.ran_commands) {
                    (0, _) => "commands only".to_string(),
                    (1, _) => "1 file".to_string(),
                    (files, _) => format!("{files} files"),
                };
                UndoSnapshot {
                    index: index as i64,
                    short_id,
                    label: turn.label.clone(),
                }
            })
            .collect()
    }

    /// Remove the undoable turn at the display index (0 = most recent) and all
    /// newer turns, returning them newest first.
    fn take_through_index(&self, index: i64) -> Result<Vec<TurnEdits>, RestoreError> {
        let mut turns = self.lock();
        let undoable: Vec<usize> = turns
            .iter()
            .enumerate()
            .filter(|(_, turn)| turn.is_undoable())
            .map(|(position, _)| position)
            .collect();
        if undoable.is_empty() {
            return Err(RestoreError::Empty);
        }
        let Some(display) = usize::try_from(index)
            .ok()
            .filter(|display| *display < undoable.len())
        else {
            return Err(RestoreError::OutOfBounds);
        };
        let cut = undoable[undoable.len() - 1 - display];
        let mut removed = turns.split_off(cut);
        removed.reverse();
        Ok(removed)
    }
}

/// What an edit's report says it replaced.
enum Before {
    /// The agent gave the replaced content.
    Replaced(String),
    /// The agent gave none: per ACP the edit creates the file.
    NewFile,
}

struct ReportedEdit {
    path: PathBuf,
    before: Before,
    after: Option<String>,
}

/// What a file held before an edit that reports no replaced content, from the edits the journal
/// already holds for the path: the newest one says what the file has held since.
fn known_before_creation(turns: &[TurnEdits], path: &Path) -> Origin {
    let newest = turns
        .iter()
        .rev()
        .flat_map(|turn| turn.calls.iter().rev())
        .flat_map(|call| call.edits.iter().rev())
        .find(|edit| edit.path == path);
    match newest {
        // Nothing was reported about the path: it is the new file the report says it is, as far as
        // the journal can tell.
        None => Origin::Created,
        // The file was deleted, so this brings it back from nothing.
        Some(FileEdit { after: None, .. }) => Origin::Known(None),
        // That edit created the file, so its result is the whole content the file then held.
        Some(FileEdit {
            before: Origin::Known(None) | Origin::Created,
            after: Some(content),
            ..
        }) => Origin::Known(Some(content.clone())),
        // The file existed, but a fragment diff does not say what it held now.
        Some(_) => Origin::Unknown,
    }
}

fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn reported_edits(snapshot: &ToolSnapshot, cwd: &Path) -> Vec<ReportedEdit> {
    let mut edits: Vec<ReportedEdit> = snapshot
        .artifacts
        .iter()
        .filter_map(|artifact| match artifact {
            Artifact::Diff(change) => Some(ReportedEdit {
                path: absolute(&change.path, cwd),
                before: match &change.old_text {
                    Some(old) => Before::Replaced(old.clone()),
                    None => Before::NewFile,
                },
                after: Some(change.new_text.clone()),
            }),
            Artifact::Text { .. } => None,
        })
        .collect();
    if !edits.is_empty() {
        return edits;
    }
    let Some(Invocation::FileOperations { operations }) = &snapshot.invocation else {
        return edits;
    };
    for operation in operations {
        match operation {
            FileOperation::Create { path, new_text } => edits.push(ReportedEdit {
                path: absolute(path, cwd),
                before: Before::NewFile,
                after: Some(new_text.clone()),
            }),
            FileOperation::Update {
                path,
                old_text,
                new_text,
            } => edits.push(ReportedEdit {
                path: absolute(path, cwd),
                before: Before::Replaced(old_text.clone()),
                after: Some(new_text.clone()),
            }),
            FileOperation::Delete { path, old_text } => {
                // Without the deleted content the file cannot be brought back.
                if let Some(old_text) = old_text {
                    edits.push(ReportedEdit {
                        path: absolute(path, cwd),
                        before: Before::Replaced(old_text.clone()),
                        after: None,
                    });
                }
            }
            FileOperation::Move {
                from_path,
                to_path,
                old_text,
                new_text,
            } => {
                if let Some(old_text) = old_text {
                    edits.push(ReportedEdit {
                        path: absolute(from_path, cwd),
                        before: Before::Replaced(old_text.clone()),
                        after: None,
                    });
                }
                if let Some(new_text) = new_text.as_ref().or(old_text.as_ref()) {
                    edits.push(ReportedEdit {
                        path: absolute(to_path, cwd),
                        before: Before::NewFile,
                        after: Some(new_text.clone()),
                    });
                }
            }
        }
    }
    edits
}

/// Write one turn's recorded content back to disk.
///
/// Edits to a path are unwound newest first. An edit is reversed when the file
/// still holds exactly what the edit produced, or, for a diff that carries only
/// the changed region, still contains that region. Anything else was changed
/// since and is left alone.
fn restore_turn(turn: &TurnEdits, trash: &UndoTrash) -> RestoreReport {
    let mut report = RestoreReport {
        ran_commands: turn.ran_commands,
        ..RestoreReport::default()
    };
    let mut by_path: Vec<(&Path, Vec<&FileEdit>)> = Vec::new();
    let mut positions: HashMap<&Path, usize> = HashMap::new();
    for edit in turn.calls.iter().flat_map(|call| call.edits.iter()) {
        let position = *positions.entry(edit.path.as_path()).or_insert_with(|| {
            by_path.push((edit.path.as_path(), Vec::new()));
            by_path.len() - 1
        });
        by_path[position].1.push(edit);
    }
    for (path, edits) in by_path {
        let mut state: Option<String> = match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                report.skipped.push((path.to_path_buf(), error.to_string()));
                continue;
            }
        };
        let mut failure = None;
        for edit in edits.iter().rev() {
            let before = match &edit.before {
                Origin::Known(before) => before.clone(),
                Origin::Created => match git_head::head_state(path) {
                    // Nothing says the file existed: it is the new file the report says it is.
                    HeadState::Untracked => None,
                    // Git has it: the file was not new, and HEAD's text is what it held.
                    HeadState::Tracked(text) => Some(text),
                    HeadState::TrackedBinary => {
                        failure = Some(
                            "git tracks it, so it was not new, but its previous content is not text"
                                .to_string(),
                        );
                        break;
                    }
                },
                Origin::Unknown => {
                    failure = Some("the original content was not reported".to_string());
                    break;
                }
            };
            // A deleted file is reported as emptied.
            if state == edit.after || (state.is_none() && edit.after.as_deref() == Some("")) {
                state = before;
            } else if let (Some(current), Some(after), Some(before)) =
                (state.as_deref(), edit.after.as_deref(), before.as_deref())
                && !after.is_empty()
                && current.contains(after)
            {
                state = Some(current.replacen(after, before, 1));
            } else {
                failure = Some("it changed after the agent's edit".to_string());
                break;
            }
        }
        if let Some(reason) = failure {
            report.skipped.push((path.to_path_buf(), reason));
            continue;
        }
        // Whatever is about to be overwritten or deleted is kept first; if it cannot be, the file
        // is left alone.
        let action = if state.is_some() {
            "overwritten"
        } else {
            "deleted"
        };
        match trash.save(turn.id, path, action) {
            Ok(Some(saved)) => report.kept.push((path.to_path_buf(), saved)),
            Ok(None) => {}
            Err(reason) => {
                report.skipped.push((path.to_path_buf(), reason));
                continue;
            }
        }
        let written = match &state {
            Some(content) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(path, content)
            }
            None => match std::fs::remove_file(path) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
                Ok(()) | Err(_) => Ok(()),
            },
        };
        match written {
            Ok(()) => report.restored.push(path.to_path_buf()),
            Err(error) => report.skipped.push((path.to_path_buf(), error.to_string())),
        }
    }
    report
}

fn completion_message(label: &str, reports: &[RestoreReport]) -> (bool, String) {
    let restored: usize = reports.iter().map(|report| report.restored.len()).sum();
    let from_snapshot: usize = reports
        .iter()
        .filter(|report| report.from_snapshot)
        .map(|report| report.restored.len())
        .sum();
    let skipped: Vec<&(PathBuf, String)> = reports
        .iter()
        .flat_map(|report| report.skipped.iter())
        .collect();
    let ran_commands = reports.iter().any(|report| report.ran_commands);
    let mut message = match restored {
        0 => format!("Undo restored no files for \"{label}\"."),
        1 => format!("Undo restored 1 file for \"{label}\"."),
        restored => format!("Undo restored {restored} files for \"{label}\"."),
    };
    if from_snapshot > 0 {
        let from_journal = restored - from_snapshot;
        message.push_str(&match from_journal {
            0 => format!(
                "\n{from_snapshot} from the turn snapshot, which also covers what commands changed."
            ),
            from_journal => format!(
                "\n{from_snapshot} from the turn snapshot, {from_journal} from the edit journal."
            ),
        });
    }
    for (path, reason) in &skipped {
        message.push_str(&format!("\nSkipped {}: {reason}.", path.display()));
    }
    let kept: Vec<&(PathBuf, PathBuf)> = reports
        .iter()
        .flat_map(|report| report.kept.iter())
        .collect();
    if !kept.is_empty() {
        message.push_str("\nWhat undo replaced or deleted was copied first (kept 14 days):");
        for (path, saved) in &kept {
            message.push_str(&format!("\n  {} -> {}", path.display(), saved.display()));
        }
    }
    if ran_commands {
        message.push_str(
            "\nThe agent also ran commands; changes made by commands were not reported as edits and are not undone.",
        );
    }
    message.push_str("\nNote: the agent is not aware that files have changed.");
    (
        skipped.is_empty() && (restored > 0 || !ran_commands),
        message,
    )
}

async fn send_started(event_tx: &mpsc::Sender<BackendEvent>, id: &str) {
    let _ = event_tx
        .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Undo(
            UndoEvent::Started {
                operation_id: id.to_string(),
                message: Some("Undo in progress...".to_string()),
            },
        ))))
        .await;
}

async fn send_completed(
    event_tx: &mpsc::Sender<BackendEvent>,
    id: &str,
    success: bool,
    message: Option<String>,
) {
    let _ = event_tx
        .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Undo(
            UndoEvent::Completed {
                operation_id: id.to_string(),
                success,
                message,
            },
        ))))
        .await;
}

/// Undo through the journal at the given display index (0 = the most recent
/// undoable turn), restoring every newer turn on the way.
///
/// Emits `UndoStarted` and `UndoCompleted` events on the provided channel.
pub async fn handle_undo_to(
    event_tx: &mpsc::Sender<BackendEvent>,
    id: &str,
    journal: &EditJournal,
    index: i64,
) {
    send_started(event_tx, id).await;
    let (success, message) = match journal.take_through_index(index) {
        Err(RestoreError::Empty) => {
            warn!("Undo requested but no edits were recorded");
            (
                false,
                "Nothing to undo: no file edits were reported this session.".to_string(),
            )
        }
        Err(RestoreError::OutOfBounds) => {
            warn!("Undo requested with out-of-bounds index {index}");
            (false, format!("Invalid undo index: {index}"))
        }
        Ok(turns) => {
            let label = turns
                .last()
                .map(|turn| turn.label.clone())
                .unwrap_or_default();
            match tree_undo::restore(journal.snapshots.as_ref(), journal.trash.clone(), turns).await
            {
                Ok(reports) => {
                    info!(turns = reports.len(), "Undo restored recorded edits");
                    completion_message(&label, &reports)
                }
                Err(error) => (false, format!("Undo failed: {error}")),
            }
        }
    };
    send_completed(event_tx, id, success, Some(message)).await;
}

/// Undo the most recent undoable turn.
pub async fn handle_undo(event_tx: &mpsc::Sender<BackendEvent>, id: &str, journal: &EditJournal) {
    handle_undo_to(event_tx, id, journal, 0).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::normalized::FileChange;
    use crate::normalized::ToolPhase;
    use pretty_assertions::assert_eq;

    fn diff_snapshot(call_id: &str, path: &Path, old: Option<&str>, new: &str) -> ToolSnapshot {
        ToolSnapshot {
            call_id: call_id.to_string(),
            title: "edit".to_string(),
            kind: ToolKind::Edit,
            phase: ToolPhase::Completed,
            locations: Vec::new(),
            invocation: None,
            artifacts: vec![Artifact::Diff(FileChange {
                path: path.to_path_buf(),
                old_text: old.map(str::to_string),
                new_text: new.to_string(),
            })],
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        }
    }

    fn command_snapshot(call_id: &str) -> ToolSnapshot {
        ToolSnapshot {
            kind: ToolKind::Execute,
            artifacts: Vec::new(),
            ..diff_snapshot(call_id, Path::new("unused"), None, "")
        }
    }

    fn restore_latest(journal: &EditJournal) -> RestoreReport {
        let turns = journal.take_through_index(0).ok().unwrap();
        turns
            .iter()
            .map(|turn| restore_turn(turn, &journal.trash))
            .next()
            .unwrap()
    }

    #[test]
    fn whole_file_edit_is_restored_and_popped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "before").unwrap();
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("edit a".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("before"), "after"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "after").unwrap();

        assert_eq!(
            journal.list(),
            vec![UndoSnapshot {
                index: 0,
                short_id: "1 file".to_string(),
                label: "edit a".to_string(),
            }]
        );
        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
        assert_eq!(journal.list(), Vec::new());
    }

    #[test]
    fn fragment_diffs_are_replaced_inside_the_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "one\ntwo-new\nthree\n").unwrap();
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("edit".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("two"), "two-new"),
            dir.path(),
            None,
        );

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\ntwo\nthree\n");
    }

    #[test]
    fn created_file_is_removed_when_it_did_not_exist_before() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("new.txt");
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("create".to_string());
        journal.record(&diff_snapshot("c1", &file, None, "hello"), dir.path(), None);
        std::fs::write(&file, "hello").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert!(!file.exists());
    }

    #[test]
    fn later_changes_by_the_user_are_not_clobbered() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("edit".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("before"), "after"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "edited by the user").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, Vec::<PathBuf>::new());
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "edited by the user"
        );
    }

    #[test]
    fn repeated_edits_to_one_path_unwind_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("two edits".to_string());
        journal.record(
            &diff_snapshot("c1", &file, Some("v0"), "v1"),
            dir.path(),
            None,
        );
        journal.record(
            &diff_snapshot("c2", &file, Some("v1"), "v2"),
            dir.path(),
            None,
        );
        std::fs::write(&file, "v2").unwrap();

        let report = restore_latest(&journal);

        assert_eq!(report.restored, vec![file.clone()]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0");
    }

    #[test]
    fn repeated_snapshots_of_one_call_are_recorded_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("edit".to_string());
        for _ in 0..3 {
            journal.record(
                &diff_snapshot("c1", &file, Some("v0"), "v1"),
                dir.path(),
                None,
            );
        }

        let turns = journal.lock();
        assert_eq!(turns[0].calls.len(), 1);
        assert_eq!(turns[0].calls[0].edits.len(), 1);
    }

    #[test]
    fn undo_to_restores_newer_turns_too_and_skips_turns_without_edits() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, "1-new").unwrap();
        std::fs::write(&second, "2-new").unwrap();
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("first".to_string());
        journal.record(
            &diff_snapshot("c1", &first, Some("1-old"), "1-new"),
            dir.path(),
            None,
        );
        journal.begin_turn("chat only".to_string());
        journal.begin_turn("second".to_string());
        journal.record(
            &diff_snapshot("c2", &second, Some("2-old"), "2-new"),
            dir.path(),
            None,
        );

        let labels: Vec<String> = journal.list().into_iter().map(|s| s.label).collect();
        assert_eq!(labels, vec!["second".to_string(), "first".to_string()]);

        let turns = journal.take_through_index(1).ok().unwrap();
        for turn in &turns {
            restore_turn(turn, &journal.trash);
        }
        let restored = turns.len();

        // The chat-only turn between the two is popped with them.
        assert_eq!(restored, 3);
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "1-old");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "2-old");
        assert!(matches!(
            journal.take_through_index(0),
            Err(RestoreError::Empty)
        ));
    }

    #[test]
    fn out_of_bounds_index_is_rejected_and_keeps_the_journal() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("edit".to_string());
        journal.record(
            &diff_snapshot("c1", &dir.path().join("a"), Some("x"), "y"),
            dir.path(),
            None,
        );

        assert!(matches!(
            journal.take_through_index(1),
            Err(RestoreError::OutOfBounds)
        ));
        assert!(matches!(
            journal.take_through_index(-1),
            Err(RestoreError::OutOfBounds)
        ));
        assert_eq!(journal.list().len(), 1);
    }

    #[test]
    fn commands_are_flagged_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.begin_turn("run it".to_string());
        journal.record(&command_snapshot("c1"), dir.path(), None);

        assert_eq!(
            journal.list(),
            vec![UndoSnapshot {
                index: 0,
                short_id: "commands only".to_string(),
                label: "run it".to_string(),
            }]
        );
        let report = restore_latest(&journal);
        let (success, message) = completion_message("run it", &[report]);
        assert!(!success);
        assert!(message.contains("not undone"), "{message}");
    }

    #[test]
    fn snapshots_without_an_open_turn_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let journal = Arc::new(EditJournal::with_trash(UndoTrash::at(
            dir.path().join("trash"),
        )));
        journal.record(
            &diff_snapshot("c1", &dir.path().join("a"), Some("x"), "y"),
            dir.path(),
            None,
        );
        assert_eq!(journal.list(), Vec::new());
    }
}
