//! Portable sessions: a whole session out of one Nori home and into another, on another machine.
//!
//! A [`SessionBundle`] is what travels: the conversation's transcript and journal and those of
//! every delegated child, and a manifest (project, origin, agent, working tree). Dials (model,
//! effort, mode) and everything else the journal records travel inside the journal and come back
//! on resume as they always do. Credentials never travel: the target uses its own sign-ins.
//!
//! - [`export`] reads a conversation from a Nori home into a bundle.
//! - [`import`] writes a bundle into a Nori home for a working directory there, and records the
//!   arrival in the journal; the conversation then resumes with `nori resume ID`.
//! - [`moving`] is the move itself: plan, ship over ssh, hand the terminal over.
//!
//! See `docs/machines.md`.

pub mod bundle;
mod export;
mod import;
pub mod moving;
#[cfg(test)]
mod tests;

pub use bundle::Manifest;
pub use bundle::SessionBundle;
pub use export::export;
pub use import::Imported;
pub use import::import;

use crate::transcript::BY_PROJECT_DIR;
use crate::transcript::SESSIONS_DIR;
use crate::transcript::TRANSCRIPTS_DIR;

#[derive(Debug, thiserror::Error)]
pub enum PortableError {
    #[error("there is no conversation {0} in this Nori home")]
    NotFound(String),
    #[error("{0} is a delegated child; move its parent conversation")]
    Child(String),
    #[error(transparent)]
    Bundle(#[from] bundle::BundleError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// The journal command a conversation records when it leaves for another machine.
pub const MOVED_OUT: &str = "moved-out";
/// The journal command a conversation records when it arrives from another machine.
pub const MOVED_IN: &str = "moved-in";
/// The journal command that binds a delegated child to its transcript.
const CHILD_BINDING: &str = "native-child-journal:";

fn sessions_dir(home: &std::path::Path, project_id: &str) -> std::path::PathBuf {
    home.join(TRANSCRIPTS_DIR)
        .join(BY_PROJECT_DIR)
        .join(project_id)
        .join(SESSIONS_DIR)
}

/// This computer's name, as `uname -n` gives it.
pub(crate) fn hostname() -> String {
    std::process::Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn now_iso8601() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Where a conversation that left this home went, and how to reach it there: kept beside its
/// transcript (`ID.moved`) so the resume list can say so without reading the journal.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MovedTo {
    pub machine: String,
    /// The ssh destination that reaches it.
    pub ssh: String,
    /// The directory it continues in there.
    pub cwd: String,
    /// A launcher there: `/bin/sh LAUNCHER` continues the conversation.
    pub launcher: Option<String>,
    pub at: String,
}

/// Where conversation `session_id` of project `project_id` went, if it left this home.
pub fn moved_to(home: &std::path::Path, project_id: &str, session_id: &str) -> Option<MovedTo> {
    let path = sessions_dir(home, project_id).join(format!("{session_id}.moved"));
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Record in conversation `conversation_id`'s journal that it left for `moved.machine`, and
/// leave the marker the resume list reads.
pub async fn record_moved_out(
    home: &std::path::Path,
    conversation_id: &str,
    moved: &MovedTo,
) -> Result<(), PortableError> {
    let machine = moved.machine.as_str();
    let cwd = moved.cwd.as_str();
    let loader = crate::transcript::TranscriptLoader::new(home.to_path_buf());
    let meta = loader
        .find_session_metadata_by_id(conversation_id)
        .await?
        .ok_or_else(|| PortableError::NotFound(conversation_id.to_owned()))?;
    let journal = loader
        .session_path(&meta.project_id, &meta.session_id)
        .with_extension("pkl");
    let payload = serde_json::json!({"to": machine, "cwd": cwd, "at": now_iso8601()}).to_string();
    let marker = journal.with_extension("moved");
    let marker_text = serde_json::to_vec_pretty(moved).unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        let mut log = nori_conversation::ConversationLog::open(&journal)?;
        log.append(nori_conversation::Event::Command {
            name: MOVED_OUT.to_owned(),
            payload,
        })?;
        log.sync()?;
        std::fs::write(marker, marker_text)
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(())
}
