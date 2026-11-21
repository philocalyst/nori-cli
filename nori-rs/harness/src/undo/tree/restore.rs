//! Putting the working tree back to a captured tree.
//!
//! The paths that differ between the target tree and a fresh capture are the paths the turns
//! since changed: files they created are removed, files they changed or deleted are written back
//! from the target tree. Whatever is overwritten or removed is first copied to the undo trash, and
//! a path whose copy cannot be made is left alone.

use std::path::Component;
use std::path::PathBuf;

use super::ChangeKind;
use super::ShadowTree;
use super::SnapshotError;
use super::TreeId;
use crate::undo::trash::UndoTrash;

/// What putting the tree back did.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct TreeRestore {
    /// Files written back or removed.
    pub restored: Vec<PathBuf>,
    /// Files undo overwrote or removed, and where their content was kept first.
    pub kept: Vec<(PathBuf, PathBuf)>,
    pub skipped: Vec<(PathBuf, String)>,
}

impl ShadowTree {
    /// Restore the work tree to `target`. `turn` names the trash directory the replaced files go
    /// to.
    pub(crate) async fn restore(
        &self,
        target: &TreeId,
        trash: &UndoTrash,
        turn: u64,
    ) -> Result<TreeRestore, SnapshotError> {
        let current = self.capture().await?;
        let changes = self.changes(target, &current).await?;
        let mut report = TreeRestore::default();
        let mut removals = Vec::new();
        let mut writes = Vec::new();
        for change in changes {
            let relative = PathBuf::from(&change.path);
            let path = self.work_tree.join(&relative);
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                report
                    .skipped
                    .push((path, "its path leaves the project".to_string()));
                continue;
            }
            match change.kind {
                ChangeKind::Added => removals.push((path, change.path)),
                ChangeKind::Modified | ChangeKind::TypeChanged | ChangeKind::Deleted => {
                    writes.push((path, change.path));
                }
            }
        }

        // Everything that exists now and is about to be replaced is kept first.
        let replaced: Vec<(PathBuf, &'static str)> = removals
            .iter()
            .map(|(path, _)| (path.clone(), "removed"))
            .chain(writes.iter().map(|(path, _)| (path.clone(), "overwritten")))
            .collect();
        let trash = trash.clone();
        let saved = tokio::task::spawn_blocking(move || {
            replaced
                .into_iter()
                .map(|(path, action)| {
                    let saved = trash.save(turn, &path, action);
                    (path, saved)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| SnapshotError::Failed {
            step: "trash",
            stderr: error.to_string(),
        })?;
        let mut unsaved = std::collections::HashSet::new();
        for (path, saved) in saved {
            match saved {
                Ok(Some(copy)) => report.kept.push((path, copy)),
                Ok(None) => {}
                Err(reason) => {
                    unsaved.insert(path.clone());
                    report.skipped.push((path, reason));
                }
            }
        }
        removals.retain(|(path, _)| !unsaved.contains(path));
        writes.retain(|(path, _)| !unsaved.contains(path));

        // Created files go first, so a directory that held only those is gone before a file of the
        // same name comes back.
        for (path, _) in removals {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    report.skipped.push((path, error.to_string()));
                    continue;
                }
            }
            let mut parent = path.parent();
            // Only an empty directory goes: `remove_dir` refuses one that holds anything.
            while let Some(directory) = parent.filter(|directory| *directory != self.work_tree) {
                if tokio::fs::remove_dir(directory).await.is_err() {
                    break;
                }
                parent = directory.parent();
            }
            report.restored.push(path);
        }

        if writes.is_empty() {
            return Ok(report);
        }
        let restore_index = self.index.with_extension("restore");
        let listing: Vec<u8> = writes
            .iter()
            .flat_map(|(_, relative)| relative.bytes().chain(std::iter::once(0)))
            .collect();
        let checked_out = async {
            self.run("read-tree", &["read-tree", &target.0], &restore_index, None)
                .await?;
            self.run(
                "checkout-index",
                &["checkout-index", "--force", "-z", "--stdin"],
                &restore_index,
                Some(listing),
            )
            .await
        }
        .await;
        let _ = tokio::fs::remove_file(&restore_index).await;
        match checked_out {
            Ok(_) => report
                .restored
                .extend(writes.into_iter().map(|(path, _)| path)),
            Err(SnapshotError::Failed { stderr, .. }) => report
                .skipped
                .extend(writes.into_iter().map(|(path, _)| (path, stderr.clone()))),
            Err(error) => return Err(error),
        }
        Ok(report)
    }
}
