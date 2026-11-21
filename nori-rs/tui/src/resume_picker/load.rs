//! Reading the rows of the resume picker: Nori's own records of the sessions of one scope,
//! most recently active first, each with the title, model, branch and turns the index holds.
//! `nori resume`, `--last` and `/resume` all read their sessions here.

use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::time::Duration;

use nori_harness::session_index::ListedSession;
use nori_harness::session_index::SessionIndex;
use nori_harness::transcript::TranscriptLoader;

use super::SessionPickerInfo;

/// How long an open picker waits, once Nori's records are in, for the agent's own listing of its
/// sessions before it opens without them; they are added when they come.
const AGENT_LISTING_GRACE: Duration = Duration::from_millis(15);

/// How long the newest sessions may take to be brought up to date before the picker opens with
/// whatever is ready: well inside the 100 ms an interaction may take. Bringing a session up to
/// date reads only what it grew by, so the newest are usually current by then; the picker
/// finishes the rest in the background and its rows update as they are.
const FRESHEN_BUDGET: Duration = Duration::from_millis(40);

#[derive(Debug, Clone, Default)]
pub(crate) struct ResumeListing {
    /// Conversations, most recently active first; delegated children and sessions in which
    /// nothing was asked are not listed.
    pub(crate) sessions: Vec<SessionPickerInfo>,
    /// Every agent session id these conversations have run in: an agent's own listing of
    /// sessions is shown only for the ones that are not here.
    pub(crate) known_acp_session_ids: HashSet<String>,
}

/// The recorded sessions in `cwd`'s project (every project when `None`), of `agent` when given.
pub(crate) async fn load(
    nori_home: &Path,
    cwd: Option<&Path>,
    agent: Option<&str>,
) -> io::Result<ResumeListing> {
    let candidates = TranscriptLoader::new(nori_home.to_path_buf())
        .list_session_heads(cwd)
        .await?;
    let listing = SessionIndex::open(nori_home)
        .listing(candidates, FRESHEN_BUDGET)
        .await?;
    // The agent a session runs on now, which a handoff may have changed since its first record:
    // the index knows it once the session is indexed.
    let runs_on = |listed: &ListedSession| {
        listed
            .header
            .as_ref()
            .and_then(|header| header.agent.clone())
            .or_else(|| listed.meta.agent.clone())
    };
    Ok(ResumeListing {
        sessions: listing
            .sessions
            .into_iter()
            .filter(|listed| agent.is_none_or(|agent| runs_on(listed).as_deref() == Some(agent)))
            .map(|listed| info_of(nori_home, listed))
            .collect(),
        known_acp_session_ids: listing.known_acp_session_ids,
    })
}

fn info_of(nori_home: &Path, listed: ListedSession) -> SessionPickerInfo {
    let mut info = SessionPickerInfo::from(listed.meta);
    info.moved_to = nori_harness::portable::moved_to(nori_home, &info.project_id, &info.session_id)
        .map(|moved| moved.machine);
    if let Some(header) = listed.header {
        info.cwd = Some(header.cwd.display().to_string());
        info.title = header.title;
        info.model = header.model;
        info.branch = header.branch;
        info.user_turn_count = Some(header.user_turns);
    }
    info
}

/// What `/resume` needs to open its picker: the records of one agent's sessions in a project,
/// and the sessions the agent itself lists that are not among them.
pub(crate) struct ResumeRequest {
    pub(crate) nori_home: std::path::PathBuf,
    pub(crate) cwd: std::path::PathBuf,
    /// The active agent's slug: only its sessions can be reopened by it.
    pub(crate) agent: String,
    /// The conversation id of the session `/resume` was typed in, to mark its row.
    pub(crate) current: Option<String>,
}

impl ResumeRequest {
    /// Load the records and open the picker with `agent_sessions`, the agent's own listing.
    pub(crate) async fn send(
        self,
        tx: crate::app_event_sender::AppEventSender,
        agent_sessions: Vec<nori_protocol::acp::v1::SessionInfo>,
    ) {
        self.send_with_agent_listing(tx, async move { agent_sessions })
            .await;
    }

    /// Load the records and open the picker as soon as they are in, with the agent's own listing
    /// if it has arrived by then. A listing that arrives later is added to the open picker
    /// (`AppEvent::ResumePickerAgentSessions`): an agent that is slow to list its sessions never
    /// holds the picker up.
    pub(crate) async fn send_with_agent_listing(
        self,
        tx: crate::app_event_sender::AppEventSender,
        agent_listing: impl std::future::Future<Output = Vec<nori_protocol::acp::v1::SessionInfo>>
        + Send
        + 'static,
    ) {
        let mut agent_listing = tokio::spawn(agent_listing);
        let listing = match load(&self.nori_home, Some(&self.cwd), Some(&self.agent)).await {
            Ok(listing) => listing,
            Err(error) => {
                agent_listing.abort();
                tx.send(crate::app_event::AppEvent::InsertHistoryCell(Box::new(
                    crate::history_cell::new_error_event(format!(
                        "Failed to load sessions: {error}"
                    )),
                )));
                return;
            }
        };
        let unknown = |sessions: Vec<nori_protocol::acp::v1::SessionInfo>| {
            sessions
                .into_iter()
                .filter(|session| {
                    !listing
                        .known_acp_session_ids
                        .contains(&session.session_id.to_string())
                })
                .collect::<Vec<_>>()
        };
        let arrived = tokio::time::timeout(AGENT_LISTING_GRACE, &mut agent_listing).await;
        let (agent_sessions, late) = match arrived {
            Ok(sessions) => (unknown(sessions.unwrap_or_default()), None),
            Err(_) => (Vec::new(), Some(agent_listing)),
        };
        tx.send(crate::app_event::AppEvent::ShowResumeSessionPicker(
            Box::new(super::helix_overlay::ResumeSetup {
                purpose: super::helix_overlay::SessionPurpose::Resume,
                nori_home: self.nori_home,
                sessions: listing.sessions,
                agent_sessions,
                current: self.current,
                offer_new: false,
                cancel_candidate_on_dismiss: false,
                // Short enough for the list's bottom edge with the key that switches to all chats.
                scope: "this project".to_owned(),
                widen: Some(super::helix_overlay::Widen {
                    cwd: self.cwd.clone(),
                    agent: self.agent.clone(),
                }),
            }),
        ));
        if let Some(late) = late
            && let Ok(sessions) = late.await
        {
            let sessions = unknown(sessions);
            if !sessions.is_empty() {
                tx.send(crate::app_event::AppEvent::ResumePickerAgentSessions(
                    sessions,
                ));
            }
        }
    }
}
