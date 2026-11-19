//! File-backed session metadata store for list, load, resume, fork, and delete.

use std::path::Path;
use std::path::PathBuf;

use antigravity_sdk_rust::types::Step;
use nori_acp_kit::schema::SessionInfo;
use serde::Deserialize;
use serde::Serialize;

pub(crate) const SESSION_META_FILE: &str = "nori_session.json";

/// Persisted session metadata for `session/list`, `session/load`, `session/resume`, and `session/fork`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistedSession {
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    #[serde(default)]
    pub(crate) additional_directories: Vec<PathBuf>,
    #[serde(default)]
    pub(crate) title: Option<String>,
    #[serde(default)]
    pub(crate) updated_at_epoch_secs: i64,
    pub(crate) model: String,
    pub(crate) thought_level: String,
    #[serde(default)]
    pub(crate) history: Vec<Step>,
}

/// File-backed store under `base_dir/<session_id>/nori_session.json`.
#[derive(Debug, Clone)]
pub(crate) struct SessionStore {
    base_dir: PathBuf,
}

impl SessionStore {
    pub(crate) fn new(explicit_base: Option<&Path>) -> Self {
        let base_dir = explicit_base.map_or_else(
            || std::env::temp_dir().join("nori-antigravity-sessions"),
            Path::to_path_buf,
        );
        Self { base_dir }
    }

    pub(crate) fn session_dir(&self, session_id: &str) -> PathBuf {
        self.base_dir.join(session_id)
    }

    pub(crate) fn save(&self, meta: &PersistedSession) -> std::io::Result<()> {
        let dir = self.session_dir(&meta.session_id);
        std::fs::create_dir_all(&dir)?;
        let bytes = serde_json::to_vec(meta).map_err(std::io::Error::other)?;
        std::fs::write(dir.join(SESSION_META_FILE), bytes)
    }

    pub(crate) fn load(&self, session_id: &str) -> Option<PersistedSession> {
        let path = self.session_dir(session_id).join(SESSION_META_FILE);
        let bytes = std::fs::read(path).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub(crate) fn delete(&self, session_id: &str) -> std::io::Result<()> {
        let dir = self.session_dir(session_id);
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    }

    pub(crate) fn list(&self, cwd_filter: Option<&Path>) -> Vec<SessionInfo> {
        let Ok(entries) = std::fs::read_dir(&self.base_dir) else {
            return Vec::new();
        };
        let canonical_filter = cwd_filter.and_then(|p| std::fs::canonicalize(p).ok());
        let mut sessions: Vec<(i64, SessionInfo)> = entries
            .flatten()
            .filter_map(|entry| {
                let meta_path = entry.path().join(SESSION_META_FILE);
                let bytes = std::fs::read(meta_path).ok()?;
                let meta: PersistedSession = serde_json::from_slice(&bytes).ok()?;
                if let Some(ref filter) = canonical_filter {
                    let meta_cwd = std::fs::canonicalize(&meta.cwd).ok()?;
                    if &meta_cwd != filter {
                        return None;
                    }
                }
                let info = SessionInfo::new(meta.session_id, meta.cwd)
                    .title(meta.title)
                    .additional_directories(meta.additional_directories);
                Some((meta.updated_at_epoch_secs, info))
            })
            .collect();
        sessions.sort_by_key(|b| std::cmp::Reverse(b.0));
        sessions.into_iter().map(|(_, info)| info).collect()
    }
}
