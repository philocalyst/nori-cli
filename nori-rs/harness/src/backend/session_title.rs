//! A title for every session, from its first prompt, kept across resumes.
//!
//! Agents rarely name their sessions while they run: Claude's and Codex's adapters send usage and
//! status in their session-info updates, never a title, so a session was untitled in the footer,
//! the terminal's tab and the status other programs read until an optional summary (a whole
//! extra agent process and a model call) finished, and for most people it never did. The first
//! prompt names the session at once instead: the same words the session index titles it with,
//! sent as a session-info update that is recorded like any other, so a resumed session and the
//! pickers find it. An agent's own title, when it sends one, is a later update and replaces it.
//!
//! A resumed conversation keeps its name: the title it last had is said again when the session
//! starts ([`recorded_title`]), however its history is replayed (by Nori from the transcript, or
//! by the agent itself, which knows nothing of a title Nori derived), and the first prompt after
//! the resume does not rename it.

use super::*;

/// The most characters of a prompt a title keeps.
const TITLE_MAX_CHARS: usize = 60;

/// Where a title Nori announces came from, in `_meta.nori.titleSource`: a frontend does not
/// announce either as news.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TitleSource {
    /// Derived from the first prompt.
    Prompt,
    /// The title a resumed conversation had.
    Recorded,
}

impl TitleSource {
    fn wire(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Recorded => "recorded",
        }
    }
}

/// A title from the words of `prompt`, without Nori's injected context, cut at a word to
/// [`TITLE_MAX_CHARS`]. `None` for a prompt with no words in it.
fn title_from_prompt(prompt: &str) -> Option<String> {
    let words = crate::injected_context::user_words(prompt);
    if words.is_empty() {
        return None;
    }
    if words.chars().count() <= TITLE_MAX_CHARS {
        return Some(words);
    }
    let mut kept = String::new();
    for word in words.split_whitespace() {
        if kept.chars().count() + word.chars().count() + 1 >= TITLE_MAX_CHARS {
            break;
        }
        if !kept.is_empty() {
            kept.push(' ');
        }
        kept.push_str(word);
    }
    if kept.is_empty() {
        kept = words.chars().take(TITLE_MAX_CHARS - 1).collect();
    }
    kept.push('…');
    Some(kept)
}

/// The session-info update that names session `session_id` `title`.
pub(super) fn title_update(
    session_id: acp::SessionId,
    title: String,
    source: TitleSource,
) -> SessionEvent {
    let mut meta = serde_json::Map::new();
    meta.insert(
        "nori".to_owned(),
        serde_json::json!({ "titleSource": source.wire() }),
    );
    SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
        acp::AgentNotification::SessionNotification(acp::SessionNotification::new(
            session_id,
            acp::SessionUpdate::SessionInfoUpdate(
                acp::SessionInfoUpdate::new().title(title).meta(meta),
            ),
        )),
    ))
}

/// The title a recorded conversation had when it was last left: the last one an update gave it
/// (the agent's own or the one derived from its first prompt), else one derived from its first
/// prompt now, for a conversation recorded before titles were.
pub(super) fn recorded_title(transcript: &crate::transcript::Transcript) -> Option<String> {
    use crate::transcript::TranscriptRecord;
    let mut title: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    for record in transcript.records() {
        match record {
            TranscriptRecord::SessionEvent(SessionEvent::Acp(
                nori_protocol::AcpEvent::Notification(acp::AgentNotification::SessionNotification(
                    notification,
                )),
            )) => {
                if let acp::SessionUpdate::SessionInfoUpdate(info) = &notification.update {
                    match info.title.as_opt_ref() {
                        Some(Some(named)) => title = Some(named.clone()),
                        Some(None) => title = None,
                        None => {}
                    }
                }
            }
            TranscriptRecord::User { content } if first_prompt.is_none() => {
                first_prompt = title_from_prompt(content);
            }
            TranscriptRecord::SessionEvent(_)
            | TranscriptRecord::User { .. }
            | TranscriptRecord::Assistant { .. }
            | TranscriptRecord::Thinking { .. } => {}
        }
    }
    title
        .filter(|title| !crate::injected_context::user_words(title).is_empty())
        .or(first_prompt)
}

impl AcpBackend {
    /// Name the session after `prompt` and tell the frontends. Nothing for a delegated child
    /// (its parent names the work), or a prompt with no words in it. The update carries
    /// `_meta.nori.titleSource = "prompt"`, so a frontend can tell it from a title the agent
    /// chose and not announce it.
    pub(super) async fn announce_derived_title(&self, prompt: &str) {
        if self.orchestrator.is_child() {
            return;
        }
        let Some(title) = title_from_prompt(prompt) else {
            return;
        };
        let event = title_update(
            self.session_id.read().await.clone(),
            title,
            TitleSource::Prompt,
        );
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(event))
            .await;
    }
}
