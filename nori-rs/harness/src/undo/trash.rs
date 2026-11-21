//! Where undo puts what it would otherwise destroy.
//!
//! Undo never deletes or overwrites a file outright. Before it does either, the file's current
//! content is copied to `$NORI_HOME/undo-trash/<journal>/<turn>/<absolute path>` and an entry is
//! appended to that turn's `manifest.jsonl`. Journals' directories older than
//! [`RETENTION`] are pruned when an undo runs. If the copy cannot be made the file is left alone.

use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

use serde_json::json;

/// How long a journal's trash is kept after it was last written.
const RETENTION: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// One journal's trash directory.
#[derive(Debug, Clone)]
pub(crate) struct UndoTrash {
    /// `<undo-trash>/<journal>`; `None` when there is no Nori home to put it in.
    root: Option<PathBuf>,
}

impl UndoTrash {
    /// The trash of the journal named `journal`, under `$NORI_HOME`.
    pub(crate) fn under_nori_home(journal: &str) -> Self {
        Self {
            root: nori_config::find_nori_home()
                .ok()
                .map(|home| home.join("undo-trash").join(journal)),
        }
    }

    /// The trash kept at `root`.
    pub(crate) fn at(root: PathBuf) -> Self {
        Self { root: Some(root) }
    }

    /// Copy the file at `path` into the trash for `turn`, noting `action` (what undo is about to
    /// do to it) in the manifest. Returns where the copy is. A file that does not exist has
    /// nothing to save and returns `Ok(None)`.
    pub(crate) fn save(
        &self,
        turn: u64,
        path: &Path,
        action: &str,
    ) -> Result<Option<PathBuf>, String> {
        if !path.exists() {
            return Ok(None);
        }
        let root = self
            .root
            .as_ref()
            .ok_or_else(|| "there is no Nori home to keep a copy in".to_string())?;
        let turn_dir = root.join(turn.to_string());
        let relative: PathBuf = path
            .components()
            .filter(|component| matches!(component, std::path::Component::Normal(_)))
            .collect();
        let saved = turn_dir.join("files").join(relative);
        let copy = || -> std::io::Result<()> {
            if let Some(parent) = saved.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(path, &saved)?;
            let entry = json!({
                "path": path,
                "action": action,
                "saved": saved,
                "at": SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |elapsed| elapsed.as_secs()),
            });
            let mut manifest = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(turn_dir.join("manifest.jsonl"))?;
            std::io::Write::write_all(&mut manifest, format!("{entry}\n").as_bytes())
        };
        copy().map_err(|error| format!("could not save a copy first ({error})"))?;
        Ok(Some(saved))
    }

    /// Remove other journals' trash that has not been written for [`RETENTION`].
    pub(crate) fn prune(&self) {
        let Some(parent) = self.root.as_deref().and_then(Path::parent) else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(parent) else {
            return;
        };
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > RETENTION);
            if stale && Some(entry.path().as_path()) != self.root.as_deref() {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
    }
}
