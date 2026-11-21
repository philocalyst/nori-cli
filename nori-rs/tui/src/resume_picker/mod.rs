//! `/resume`: pick a recorded session. One Helix picker ([`helix_overlay::ResumeOverlay`]) over
//! the sessions of a project, most recently active first, searched by fuzzy fields and by full
//! text from the on-disk index (see `nori_harness::session_index`), with a preview of the
//! selected conversation. Every row resumes into a live session.
//!
//! [`run_resume_picker`] hosts the picker before the app starts (`nori resume`); inside the app
//! the same overlay is shown by the app's overlay machinery. [`load`] reads the rows.

use std::path::Path;
use std::path::PathBuf;

use color_eyre::eyre::Result;
use nori_harness::transcript::SessionMetadata;
use tokio_stream::StreamExt;

use crate::tui::Tui;

mod background;
mod helix_entry;
pub(crate) mod helix_overlay;
pub(crate) mod load;
mod place;
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
    /// When anything was last recorded in it (ISO 8601): what rows are ordered and dated by.
    pub last_active_at: String,
    /// The agent (and so the account) that recorded it, as its slug.
    pub agent: Option<String>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub branch: Option<String>,
    /// What it was about, once the index has read it.
    pub title: Option<String>,
    /// Exact number of user turns, once known.
    pub user_turn_count: Option<i64>,
    /// The machine it moved to, when it left this home.
    pub moved_to: Option<String>,
}

impl From<SessionMetadata> for SessionPickerInfo {
    fn from(session: SessionMetadata) -> Self {
        Self {
            session_id: session.session_id,
            project_id: session.project_id,
            started_at: session.started_at,
            last_active_at: session.last_active_at,
            agent: session.agent,
            cwd: Some(session.cwd.display().to_string()),
            model: None,
            branch: None,
            title: None,
            user_turn_count: None,
            moved_to: None,
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
    /// Esc or Ctrl-C in the startup picker: `nori resume` ends without starting anything.
    Exit,
}

/// Interactive session picker for `nori resume`, over the sessions `listing` read: the current
/// project's (or every project's), optionally of one agent. Esc or Ctrl-C leaves without
/// starting a session; choosing a row resumes it under the agent that recorded it.
pub async fn run_resume_picker(
    tui: &mut Tui,
    nori_home: &Path,
    listing: load::ResumeListing,
    scope: &str,
) -> Result<ResumeSelection> {
    let mut overlay = helix_overlay::ResumeOverlay::new(helix_overlay::ResumeSetup {
        purpose: helix_overlay::SessionPurpose::Resume,
        nori_home: nori_home.to_path_buf(),
        sessions: listing.sessions,
        agent_sessions: Vec::new(),
        current: None,
        offer_new: false,
        cancel_candidate_on_dismiss: false,
        scope: scope.to_owned(),
        widen: None,
    });

    tui.frame_requester().schedule_frame();
    let mut tui_events = tui.event_stream().fuse();
    loop {
        let Some(event) = tui_events.next().await else {
            return Ok(ResumeSelection::Exit);
        };
        overlay.handle_event(tui, event)?;
        match overlay.take_outcome() {
            helix_overlay::ResumeOutcome::Pending => {}
            helix_overlay::ResumeOutcome::Cancelled { .. } => return Ok(ResumeSelection::Exit),
            helix_overlay::ResumeOutcome::Selected(action) => match action {
                helix_overlay::ResumeAction::Local {
                    nori_home,
                    project_id,
                    session_id,
                    agent,
                } => {
                    return Ok(ResumeSelection::Resume(ResumeTarget {
                        nori_home,
                        project_id,
                        session_id,
                        agent,
                    }));
                }
                helix_overlay::ResumeAction::StartFresh
                | helix_overlay::ResumeAction::Acp { .. }
                | helix_overlay::ResumeAction::Current
                | helix_overlay::ResumeAction::Reference { .. } => {
                    unreachable!("the startup picker lists only recorded sessions")
                }
            },
        }
    }
}
