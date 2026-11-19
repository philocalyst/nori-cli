//! One session's state and its persistence.

use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use antigravity_sdk_rust::types::Step;
use nori_acp_kit::HostBridge;
use nori_acp_kit::Options;
use nori_acp_kit::TurnGate;
use tokio::sync::Mutex;

use crate::connection::Connection;
use crate::models;
use crate::store::PersistedSession;
use crate::store::SessionStore;

/// Live state for one Antigravity session.
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) options: Mutex<Options>,
    pub(crate) host: Option<HostBridge>,
    pub(crate) conn: Connection,
    pub(crate) history: Mutex<Vec<Step>>,
    pub(crate) title: Mutex<Option<String>>,
    pub(crate) save_dir: PathBuf,
    pub(crate) gate: TurnGate,
}

impl Session {
    pub(crate) async fn persist(&self, store: &SessionStore) {
        let options = self.options.lock().await.clone();
        let meta = PersistedSession {
            session_id: self.id.clone(),
            cwd: self.cwd.clone(),
            additional_directories: self.additional_directories.clone(),
            title: self.title.lock().await.clone(),
            updated_at_epoch_secs: now_epoch_secs(),
            model: models::model(&options),
            thought_level: models::thought_level(&options),
            history: self.history.lock().await.clone(),
        };
        if let Err(error) = store.save(&meta) {
            tracing::warn!(session_id = %self.id, %error, "failed to persist Antigravity session");
        }
    }

    /// Stop the harness for this session.
    pub(crate) async fn stop(&self) {
        self.gate.cancel();
        self.conn.halt().await;
        self.conn.disconnect().await;
    }
}

/// Generate a 36-character `[a-z0-9-]` session ID matching `localharness`'s 32+ char requirement.
pub(crate) fn generate_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0_u128, |d| d.as_nanos());
    let seq = u128::from(SEQ.fetch_add(1, Ordering::Relaxed));
    let pid = u128::from(std::process::id());
    let mixed = nanos ^ (pid << 64) ^ (seq << 96);
    let hi = (mixed >> 64) as u64;
    let lo = mixed as u64;
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (hi >> 32) as u32,
        ((hi >> 16) & 0xffff) as u16,
        (hi & 0x0fff) as u16,
        ((lo >> 48) & 0x0fff) as u16,
        lo & 0x0000_ffff_ffff_ffff,
    )
}

fn now_epoch_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}
