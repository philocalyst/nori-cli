//! Entries created, modified and deleted under some paths.
//!
//! The tree is read on an interval and compared with the last reading: no platform event API, so
//! it behaves the same everywhere and a burst of changes is one reading. The monitor's `batch_ms`
//! is the debounce. Directories that are never interesting (`.git`, `node_modules`, `target`)
//! are skipped below the watched roots; a root named explicitly is always watched.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::monitor::event::Ending;
use crate::backend::monitor::spec::ChangeKind;

const POLL: Duration = Duration::from_millis(500);
/// A tree with more entries than this is not watched: say so, instead of reading it forever.
const MAX_ENTRIES: usize = 50_000;
const SKIPPED: [&str; 5] = [".git", "node_modules", "target", ".direnv", "__pycache__"];

/// What one entry looked like: when it changed, how big it was.
type Reading = BTreeMap<PathBuf, (Option<SystemTime>, u64)>;

pub(super) async fn run(
    paths: Vec<PathBuf>,
    kinds: &[ChangeKind],
    tx: &Sender<Item>,
) -> Option<Ending> {
    let mut before = match read(paths.clone()).await {
        Ok(reading) => reading,
        Err(ending) => return Some(ending),
    };
    loop {
        tokio::time::sleep(POLL).await;
        let after = match read(paths.clone()).await {
            Ok(reading) => reading,
            Err(ending) => return Some(ending),
        };
        let mut changes: Vec<(ChangeKind, &Path)> = Vec::new();
        for (path, now) in &after {
            match before.get(path) {
                None => changes.push((ChangeKind::Created, path)),
                Some(then) if then != now => changes.push((ChangeKind::Modified, path)),
                Some(_) => {}
            }
        }
        changes.extend(
            before
                .keys()
                .filter(|path| !after.contains_key(*path))
                .map(|path| (ChangeKind::Deleted, path.as_path())),
        );
        for (kind, path) in changes {
            // A directory's own mtime moves whenever something inside it does: that is the
            // entries' news, not the directory's.
            if kind == ChangeKind::Modified && path.is_dir() {
                continue;
            }
            if kinds.contains(&kind) {
                let text = format!("{} {}", kind.verb(), shown(&paths, path));
                if tx
                    .send(Item::Line {
                        text,
                        failure: false,
                    })
                    .await
                    .is_err()
                {
                    return None;
                }
            }
        }
        before = after;
    }
}

/// `path` relative to the watched root it is under.
fn shown(roots: &[PathBuf], path: &Path) -> String {
    roots
        .iter()
        .find(|root| path.starts_with(root))
        .map_or_else(
            || path.display().to_string(),
            |root| match path.strip_prefix(root) {
                Ok(rest) if rest.as_os_str().is_empty() => root.display().to_string(),
                Ok(rest) => root.join(rest).display().to_string(),
                Err(_) => path.display().to_string(),
            },
        )
}

async fn read(paths: Vec<PathBuf>) -> Result<Reading, Ending> {
    let reading = tokio::task::spawn_blocking(move || {
        let mut reading = Reading::new();
        for root in &paths {
            walk(root, true, &mut reading)?;
        }
        Ok(reading)
    })
    .await;
    match reading {
        Ok(Ok(reading)) => Ok(reading),
        Ok(Err(reason)) => Err(Ending::Failed { reason }),
        Err(error) => Err(Ending::Failed {
            reason: error.to_string(),
        }),
    }
}

fn walk(path: &Path, root: bool, reading: &mut Reading) -> Result<(), String> {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        // Gone between the listing and now: the next reading says so.
        return Ok(());
    };
    if reading.len() >= MAX_ENTRIES {
        return Err(format!(
            "more than {MAX_ENTRIES} entries under the watched paths: watch fewer, or narrower, paths"
        ));
    }
    reading.insert(
        path.to_path_buf(),
        (metadata.modified().ok(), metadata.len()),
    );
    if !metadata.is_dir() {
        return Ok(());
    }
    let skipped = !root
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| SKIPPED.contains(&name));
    if skipped {
        return Ok(());
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        walk(&entry.path(), false, reading)?;
    }
    Ok(())
}
