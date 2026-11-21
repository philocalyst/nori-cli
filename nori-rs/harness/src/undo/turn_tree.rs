//! The journal's use of shadow-tree snapshots: what a turn's start and end looked like.
//!
//! The tree before a turn's prompt is sent is captured on that turn's path (nothing the agent does
//! can race into it); to keep that wait short the shadow index is kept warm, by one capture when
//! the session starts and another whenever a turn ends, both in the background. A capture that
//! fails leaves its turn on the diff journal.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use tokio::sync::mpsc;
use tracing::debug;
use tracing::warn;

use super::EditJournal;
use super::tree::Limits;
use super::tree::ShadowTree;
use super::tree::SnapshotError;
use super::tree::TreeId;
use crate::backend::BackendEvent;

#[cfg(test)]
mod tests;

/// A turn-start capture slower than this is reported: it is time the prompt waited.
const SLOW_TURN_START: Duration = Duration::from_millis(250);
/// A turn-start capture that takes this long is given up on, and its turn stays on the journal:
/// the prompt never waits longer than this for a snapshot (a cold first capture of a large tree
/// runs on in the background and warms the index for the next turn).
const TURN_START_LIMIT: Duration = Duration::from_millis(400);

/// What the shadow tree saw of one turn.
#[derive(Debug, Default)]
pub(super) struct TurnTrees {
    /// The tree before the turn's prompt was sent.
    pub(super) start: Option<TreeId>,
    /// The tree after the turn ended; absent while it runs.
    pub(super) end: Option<TreeId>,
    /// How many files differ between the two.
    pub(super) changed_files: Option<i64>,
}

impl TurnTrees {
    /// Whether the turn changed any file the snapshot sees, once both trees are known.
    pub(super) fn changed(&self) -> Option<bool> {
        Some(self.start.as_ref()? != self.end.as_ref()?)
    }
}

/// A journal's lazily opened shadow tree.
#[derive(Clone)]
pub(super) struct TurnSnapshots {
    inner: Arc<Inner>,
}

struct Inner {
    cwd: PathBuf,
    nori_home: PathBuf,
    events: mpsc::Sender<BackendEvent>,
    /// `None` inside: snapshots are not available for this session.
    tree: tokio::sync::OnceCell<Option<Arc<ShadowTree>>>,
    noticed: AtomicBool,
}

impl TurnSnapshots {
    fn new(cwd: &Path, nori_home: &Path, events: mpsc::Sender<BackendEvent>) -> Self {
        Self {
            inner: Arc::new(Inner {
                cwd: cwd.to_path_buf(),
                nori_home: nori_home.to_path_buf(),
                events,
                tree: tokio::sync::OnceCell::new(),
                noticed: AtomicBool::new(false),
            }),
        }
    }

    /// The shadow tree, opened on first use; `None` when the directory is not in a git work tree
    /// or git is missing.
    pub(super) async fn tree(&self) -> Option<&Arc<ShadowTree>> {
        self.inner
            .tree
            .get_or_init(|| async {
                match ShadowTree::open(&self.inner.cwd, &self.inner.nori_home, Limits::DEFAULT)
                    .await
                {
                    Ok(tree) => {
                        let tree = Arc::new(tree);
                        let maintenance = Arc::clone(&tree);
                        tokio::spawn(async move { maintenance.maintain().await });
                        Some(tree)
                    }
                    Err(SnapshotError::NoGit) => {
                        debug!("undo snapshots are not available here: not a git work tree");
                        None
                    }
                    Err(error) => {
                        warn!("undo snapshots are not available: {error}");
                        None
                    }
                }
            })
            .await
            .as_ref()
    }

    /// Capture the tree now, or say why not. A tree too big to snapshot is announced once.
    pub(super) async fn capture(&self) -> Option<TreeId> {
        let tree = self.tree().await?;
        match tree.capture().await {
            Ok(id) => Some(id),
            Err(SnapshotError::TooLarge { reason }) => {
                if !self.inner.noticed.swap(true, Ordering::Relaxed) {
                    warn!("undo snapshots are off for this session: {reason}");
                    let _ = self
                        .inner
                        .events
                        .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                            nori_protocol::Notice {
                                message: format!(
                                    "Undo snapshots are off for this session ({reason}). /undo covers only the edits the agent reported."
                                ),
                            },
                        ))))
                        .await;
                }
                None
            }
            Err(error @ (SnapshotError::NoGit | SnapshotError::Failed { .. })) => {
                warn!("could not snapshot the working tree: {error}");
                None
            }
        }
    }
}

impl EditJournal {
    /// A journal that also snapshots the tree of the git work tree containing `cwd` under
    /// `nori_home`, and warms the snapshot's index in the background. `events` carries the one
    /// notice a project too big to snapshot earns.
    pub fn for_cwd(cwd: &Path, nori_home: &Path, events: mpsc::Sender<BackendEvent>) -> Self {
        let snapshots = TurnSnapshots::new(cwd, nori_home, events);
        if tokio::runtime::Handle::try_current().is_ok() {
            let warm = snapshots.clone();
            tokio::spawn(async move {
                warm.capture().await;
            });
        }
        Self {
            snapshots: Some(snapshots),
            ..Self::with_trash(super::trash::UndoTrash::at(
                nori_home
                    .join("undo-trash")
                    .join(uuid::Uuid::new_v4().to_string()),
            ))
        }
    }

    /// The id of the turn opened last.
    pub(crate) fn latest_turn_id(&self) -> Option<u64> {
        self.lock().last().map(|turn| turn.id)
    }

    /// Record the tree before `turn`'s prompt is sent. Waits for the capture, which is the point;
    /// a capture that fails or takes too long leaves the turn on the diff journal.
    pub(crate) async fn capture_turn_start(&self, turn: u64) {
        let Some(snapshots) = &self.snapshots else {
            return;
        };
        let started = Instant::now();
        let captured = tokio::time::timeout(TURN_START_LIMIT, snapshots.capture())
            .await
            .ok()
            .flatten();
        let elapsed = started.elapsed();
        if elapsed > SLOW_TURN_START {
            warn!(
                elapsed_ms = elapsed.as_millis() as i64,
                "the turn-start snapshot delayed the prompt"
            );
        }
        if let Some(tree) = captured
            && let Some(turn) = self.lock().iter_mut().find(|entry| entry.id == turn)
        {
            turn.trees.start = Some(tree);
        }
    }

    /// In the background: record the tree after the latest turn ended, and leave the shadow index
    /// warm for the next turn's start.
    pub(crate) fn capture_turn_end(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let journal = Arc::clone(self);
        // Named now: a follow-up prompt may open the next turn before the capture is done.
        let ended = self.latest_turn_id();
        tokio::spawn(async move {
            let (Some(snapshots), Some(turn)) = (&journal.snapshots, ended) else {
                return;
            };
            let Some(end) = snapshots.capture().await else {
                return;
            };
            let start = journal
                .lock()
                .iter()
                .find(|entry| entry.id == turn)
                .and_then(|entry| entry.trees.start.clone());
            let changed_files = match (start, snapshots.tree().await) {
                (Some(start), Some(tree)) => tree
                    .changes(&start, &end)
                    .await
                    .ok()
                    .map(|changes| changes.len() as i64),
                _ => None,
            };
            if let Some(entry) = journal.lock().iter_mut().find(|entry| entry.id == turn)
                && entry.trees.end.is_none()
            {
                entry.trees.end = Some(end);
                entry.trees.changed_files = changed_files;
            }
        })
    }
}
