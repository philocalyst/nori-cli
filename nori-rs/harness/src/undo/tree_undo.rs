//! Undoing turns, from the tree snapshot where there is one and from the diff journal where not.
//!
//! Restoring the tree captured at the start of the oldest turn being undone puts back everything
//! that turn and every later one changed, however they changed it. Turns older than that one
//! (their capture failed) fall back to what the agent reported, applied to the restored tree.

use super::RestoreReport;
use super::TurnEdits;
use super::restore_turn;
use super::trash::UndoTrash;
use tracing::warn;

/// Restore `turns` (newest first) and say what each restoration did. `snapshots` is the
/// journal's snapshot source, if it has one.
pub(super) async fn restore(
    snapshots: Option<&super::turn_tree::TurnSnapshots>,
    trash: UndoTrash,
    turns: Vec<TurnEdits>,
) -> Result<Vec<RestoreReport>, tokio::task::JoinError> {
    let mut reports = Vec::new();
    let mut remaining = turns;
    if let Some(snapshots) = snapshots
        && let Some(oldest_snapshotted) = remaining
            .iter()
            .rposition(|turn| turn.trees.start.is_some())
        && let Some(tree) = snapshots.tree().await
    {
        // The turns from this one on are covered by its tree; older ones stay for the journal.
        let older = remaining.split_off(oldest_snapshotted + 1);
        let covered = std::mem::replace(&mut remaining, older);
        if let Some(turn) = covered.last()
            && let Some(start) = &turn.trees.start
        {
            match tree.restore(start, &trash, turn.id).await {
                Ok(restored) => reports.push(RestoreReport {
                    restored: restored.restored,
                    skipped: restored.skipped,
                    kept: restored.kept,
                    from_snapshot: true,
                    ran_commands: false,
                }),
                Err(error) => {
                    warn!("could not restore the turn snapshot, using the edit journal: {error}");
                    remaining = covered.into_iter().chain(remaining).collect();
                }
            }
        }
    }
    let from_journal = tokio::task::spawn_blocking(move || {
        trash.prune();
        remaining
            .iter()
            .map(|turn| restore_turn(turn, &trash))
            .collect::<Vec<_>>()
    })
    .await?;
    reports.extend(from_journal);
    Ok(reports)
}
