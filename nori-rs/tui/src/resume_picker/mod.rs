//! `/resume`: pick a recorded session. One Helix picker ([`helix_overlay::ResumeOverlay`]) over
//! the sessions of a project, searched by fuzzy fields and by full text from the on-disk index
//! (see `nori_harness::session_index`), with a preview of the selected conversation.
//!
//! [`run_resume_picker`] hosts the picker before the app starts (`nori resume`); inside the app
//! the same overlay is shown by the app's overlay machinery.

use std::path::Path;
use std::path::PathBuf;

use color_eyre::eyre::Result;
use nori_harness::transcript::SessionMetadata;
use nori_harness::transcript::TranscriptLoader;
use tokio_stream::StreamExt;

use crate::tui::Tui;

mod background;
mod helix_entry;
pub(crate) mod helix_overlay;
mod preview;
#[cfg(feature = "storybook")]
pub mod specimen;

/// A recorded session as the pickers list it.
#[derive(Debug, Clone)]
pub struct SessionPickerInfo {
    pub session_id: String,
    pub project_id: String,
    /// When the session started (ISO 8601).
    pub started_at: String,
    /// Exact number of user turns, once known.
    pub user_turn_count: Option<usize>,
    /// The first user message, once known.
    pub first_message_preview: Option<String>,
    /// A delegated child: shown under its parent, never opened on its own.
    pub delegated: bool,
}

impl From<SessionMetadata> for SessionPickerInfo {
    fn from(session: SessionMetadata) -> Self {
        Self {
            session_id: session.session_id,
            project_id: session.project_id,
            started_at: session.started_at,
            user_turn_count: None,
            first_message_preview: None,
            delegated: session.delegated,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ResumeTarget {
    pub nori_home: PathBuf,
    pub project_id: String,
    pub session_id: String,
    pub agent: Option<String>,
}

#[derive(Debug, Clone)]
pub enum ResumeSelection {
    StartFresh,
    Resume(ResumeTarget),
    /// Ctrl-C in the picker: quit Nori.
    Exit,
}

/// Interactive session picker for `nori resume`: lists the recorded sessions of the current
/// project (or all with `show_all`), optionally of one agent.
pub async fn run_resume_picker(
    tui: &mut Tui,
    nori_home: &Path,
    agent_filter: Option<&str>,
    show_all: bool,
) -> Result<ResumeSelection> {
    let filter_cwd = if show_all {
        None
    } else {
        std::env::current_dir().ok()
    };
    let loader = TranscriptLoader::new(nori_home.to_path_buf());
    let sessions: Vec<SessionPickerInfo> = loader
        .list_resumable_session_metadata(filter_cwd.as_deref(), agent_filter)
        .await?
        .into_iter()
        .map(SessionPickerInfo::from)
        .collect();
    let mut overlay = helix_overlay::ResumeOverlay::new_local(sessions, nori_home.to_path_buf());

    tui.frame_requester().schedule_frame();
    let mut tui_events = tui.event_stream().fuse();
    loop {
        let Some(event) = tui_events.next().await else {
            return Ok(ResumeSelection::StartFresh);
        };
        overlay.handle_event(tui, event)?;
        match overlay.take_outcome() {
            helix_overlay::ResumeOutcome::Pending => {}
            helix_overlay::ResumeOutcome::Cancelled { .. } => {
                return Ok(if overlay.was_interrupted() {
                    ResumeSelection::Exit
                } else {
                    ResumeSelection::StartFresh
                });
            }
            helix_overlay::ResumeOutcome::Selected(action) => match action {
                helix_overlay::ResumeAction::StartFresh
                | helix_overlay::ResumeAction::Acp { .. }
                | helix_overlay::ResumeAction::ViewOnly { .. }
                | helix_overlay::ResumeAction::Reference { .. } => {
                    return Ok(ResumeSelection::StartFresh);
                }
                helix_overlay::ResumeAction::Local {
                    nori_home,
                    project_id,
                    session_id,
                } => {
                    return Ok(ResumeSelection::Resume(ResumeTarget {
                        nori_home,
                        project_id,
                        session_id,
                        agent: agent_filter.map(str::to_owned),
                    }));
                }
            },
        }
    }
}
