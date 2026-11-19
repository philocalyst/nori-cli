//! The normalized event stream of a session, and a query API over it.
//!
//! [`Events`] is a snapshot of every `SessionEvent` the harness emitted, in order. It answers the
//! questions a test asks: what did the root say, what did this subagent do, which tools never
//! ended, what permission was asked and how was it answered.
//!
//! ```ignore
//! let events = world.events();
//! assert_eq!(events.subagent("reviewer")?.tool_calls().len(), 2);
//! assert!(events.root().text().contains("all done"));
//! ```

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

/// One event and where it falls.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub seq: i64,
    /// Time since the world booted.
    pub at: Duration,
    pub event: SessionEvent,
}

/// A snapshot of a session's events.
#[derive(Debug, Clone, Default)]
pub struct Events {
    items: Vec<Arc<Recorded>>,
}

/// A tool call as the client saw it: the announcement and every update, folded.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolView {
    pub id: String,
    pub title: String,
    pub kind: acp::ToolKind,
    /// The latest status any update reported.
    pub status: acp::ToolCallStatus,
    /// Statuses in the order they arrived, announcement first.
    pub statuses: Vec<acp::ToolCallStatus>,
    pub locations: Vec<String>,
    pub raw_input: Option<serde_json::Value>,
    pub raw_output: Option<serde_json::Value>,
    /// Text content of the latest update that carried content.
    pub text: String,
    /// `(path, old_text, new_text)` of diff content.
    pub diffs: Vec<(String, Option<String>, String)>,
    /// Updates arrived for this call before it was announced.
    pub updated_before_announced: bool,
}

impl ToolView {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
        )
    }
}

/// A permission request the harness handed the client.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionView {
    pub request_id: acp::RequestId,
    pub session_id: acp::SessionId,
    pub tool_call_id: String,
    pub title: String,
    pub options: Vec<(String, acp::PermissionOptionKind)>,
    pub seq: i64,
}

/// What the client knows of a subagent.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentInfo {
    pub session_id: acp::SessionId,
    pub parent: acp::SessionId,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub state: Option<SubagentState>,
}

impl SubagentInfo {
    fn is_named(&self, who: &str) -> bool {
        self.session_id.to_string() == who
            || self.agent.as_deref() == Some(who)
            || self
                .title
                .as_deref()
                .is_some_and(|title| title.contains(who))
    }
}

/// The updates of one session, in order: the root's or a child's.
#[derive(Debug, Clone)]
pub struct Lane<'a> {
    pub session: Option<acp::SessionId>,
    updates: Vec<(&'a Arc<Recorded>, &'a acp::SessionUpdate)>,
}

impl Events {
    pub fn new(items: Vec<Arc<Recorded>>) -> Self {
        Self { items }
    }

    pub fn all(&self) -> &[Arc<Recorded>] {
        &self.items
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn nori(&self) -> impl Iterator<Item = &NoriEvent> {
        self.items.iter().filter_map(|item| match &item.event {
            SessionEvent::Nori(event) => Some(event),
            SessionEvent::Acp(_) => None,
        })
    }

    /// Events after `seq`, for "what happened since".
    pub fn since(&self, seq: i64) -> Self {
        Self::new(
            self.items
                .iter()
                .filter(|item| item.seq > seq)
                .cloned()
                .collect(),
        )
    }

    /// The root session's id, once it started.
    pub fn root_session(&self) -> Option<acp::SessionId> {
        self.nori().find_map(|event| match event {
            NoriEvent::SessionStarted(started) => Some(started.acp_session_id.clone()),
            _ => None,
        })
    }

    /// The root conversation's updates.
    pub fn root(&self) -> Lane<'_> {
        let updates = self
            .items
            .iter()
            .filter_map(|item| match &item.event {
                SessionEvent::Acp(AcpEvent::Notification(
                    acp::AgentNotification::SessionNotification(notification),
                )) => Some((item, &notification.update)),
                _ => None,
            })
            .collect();
        Lane {
            session: self.root_session(),
            updates,
        }
    }

    /// The updates the client received for child session `session`.
    pub fn child(&self, session: &acp::SessionId) -> Lane<'_> {
        let updates = self
            .items
            .iter()
            .filter_map(|item| match &item.event {
                SessionEvent::Acp(AcpEvent::ChildNotification(notification))
                    if &notification.session_id == session =>
                {
                    Some((item, &notification.update))
                }
                _ => None,
            })
            .collect();
        Lane {
            session: Some(session.clone()),
            updates,
        }
    }

    /// Every subagent announced, with the latest of what was said about it.
    pub fn subagents(&self) -> Vec<SubagentInfo> {
        let mut by_session: BTreeMap<String, SubagentInfo> = BTreeMap::new();
        let mut order = Vec::new();
        for event in self.nori() {
            let NoriEvent::Subagent(SubagentWire::Update(update)) = event else {
                continue;
            };
            let key = update.session_id.to_string();
            let info = by_session.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                SubagentInfo {
                    session_id: update.session_id.clone(),
                    parent: update.parent_session_id.clone(),
                    agent: None,
                    model: None,
                    title: None,
                    description: None,
                    state: None,
                }
            });
            apply(&update.agent, &mut info.agent);
            apply(&update.model, &mut info.model);
            apply(&update.title, &mut info.title);
            apply(&update.description, &mut info.description);
            apply(&update.state, &mut info.state);
        }
        order
            .into_iter()
            .filter_map(|key| by_session.remove(&key))
            .collect()
    }

    /// A subagent by agent slug, a fragment of its title, or its session id.
    pub fn subagent(&self, who: &str) -> Option<Lane<'_>> {
        let info = self
            .subagents()
            .into_iter()
            .find(|info| info.is_named(who))?;
        Some(self.child(&info.session_id))
    }

    pub fn permission_requests(&self) -> Vec<PermissionView> {
        self.items
            .iter()
            .filter_map(|item| match &item.event {
                SessionEvent::Acp(AcpEvent::Request {
                    request_id,
                    request: acp::AgentRequest::RequestPermissionRequest(request),
                }) => Some(PermissionView {
                    request_id: request_id.clone(),
                    session_id: request.session_id.clone(),
                    tool_call_id: request.tool_call.tool_call_id.to_string(),
                    title: request.tool_call.fields.title.clone().unwrap_or_default(),
                    options: request
                        .options
                        .iter()
                        .map(|option| (option.option_id.to_string(), option.kind))
                        .collect(),
                    seq: item.seq,
                }),
                _ => None,
            })
            .collect()
    }

    pub fn phases(&self) -> Vec<SessionPhase> {
        self.nori()
            .filter_map(|event| match event {
                NoriEvent::SessionPhaseChanged(phase) => Some(phase.clone()),
                _ => None,
            })
            .collect()
    }

    /// The phase the root session is in now.
    pub fn phase(&self) -> Option<SessionPhase> {
        self.phases().pop()
    }

    pub fn failures(&self) -> Vec<&nori_protocol::RequestFailure> {
        self.nori()
            .filter_map(|event| match event {
                NoriEvent::RequestFailed(failure) => Some(failure),
                _ => None,
            })
            .collect()
    }

    pub fn route_changes(&self) -> Vec<&nori_protocol::RouteChanged> {
        self.nori()
            .filter_map(|event| match event {
                NoriEvent::RouteChanged(change) => Some(change),
                _ => None,
            })
            .collect()
    }

    /// Prompt responses the client saw, by request id.
    pub fn responses(&self) -> Vec<(acp::RequestId, bool)> {
        self.items
            .iter()
            .filter_map(|item| match &item.event {
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response,
                }) => Some((request_id.clone(), response.is_ok())),
                _ => None,
            })
            .collect()
    }
}

fn apply<T: Clone>(patch: &Patch<T>, current: &mut Option<T>) {
    match patch {
        Patch::Keep => {}
        Patch::Clear => *current = None,
        Patch::Set(value) => *current = Some(value.clone()),
    }
}

impl Lane<'_> {
    pub fn len(&self) -> usize {
        self.updates.len()
    }

    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }

    pub fn updates(&self) -> impl DoubleEndedIterator<Item = &acp::SessionUpdate> {
        self.updates.iter().map(|(_, update)| *update)
    }

    fn chunks(&self, thought: bool) -> String {
        self.updates()
            .filter_map(|update| match (update, thought) {
                (acp::SessionUpdate::AgentMessageChunk(chunk), false)
                | (acp::SessionUpdate::AgentThoughtChunk(chunk), true) => match &chunk.content {
                    acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    /// The assistant's text, chunks joined.
    pub fn text(&self) -> String {
        self.chunks(false)
    }

    pub fn thoughts(&self) -> String {
        self.chunks(true)
    }

    /// The latest plan: `(content, status)` of each entry.
    pub fn plan(&self) -> Option<Vec<(String, acp::PlanEntryStatus)>> {
        self.updates().rev().find_map(|update| match update {
            acp::SessionUpdate::Plan(plan) => Some(
                plan.entries
                    .iter()
                    .map(|entry| (entry.content.clone(), entry.status.clone()))
                    .collect(),
            ),
            _ => None,
        })
    }

    /// Every tool call, in announcement order, with its updates folded in.
    pub fn tool_calls(&self) -> Vec<ToolView> {
        let mut order: Vec<String> = Vec::new();
        let mut views: BTreeMap<String, ToolView> = BTreeMap::new();
        for update in self.updates() {
            match update {
                acp::SessionUpdate::ToolCall(call) => {
                    let id = call.tool_call_id.to_string();
                    let early = views.get(&id).is_some_and(|view| view.statuses.is_empty());
                    let view = views.entry(id.clone()).or_insert_with(|| {
                        order.push(id.clone());
                        blank(&id)
                    });
                    view.updated_before_announced |= early;
                    view.title = call.title.clone();
                    view.kind = call.kind;
                    view.statuses.push(call.status);
                    view.status = call.status;
                    view.locations = call
                        .locations
                        .iter()
                        .map(|location| location.path.display().to_string())
                        .collect();
                    view.raw_input = call.raw_input.clone();
                    if call.raw_output.is_some() {
                        view.raw_output = call.raw_output.clone();
                    }
                    fold_content(view, &call.content);
                }
                acp::SessionUpdate::ToolCallUpdate(update) => {
                    let id = update.tool_call_id.to_string();
                    let announced = views.contains_key(&id);
                    let view = views.entry(id.clone()).or_insert_with(|| {
                        order.push(id.clone());
                        blank(&id)
                    });
                    view.updated_before_announced |= !announced;
                    if let Some(status) = update.fields.status {
                        view.statuses.push(status);
                        view.status = status;
                    }
                    if let Some(title) = &update.fields.title {
                        view.title = title.clone();
                    }
                    if let Some(kind) = update.fields.kind {
                        view.kind = kind;
                    }
                    if let Some(raw_output) = &update.fields.raw_output {
                        view.raw_output = Some(raw_output.clone());
                    }
                    if let Some(content) = &update.fields.content {
                        fold_content(view, content);
                    }
                }
                _ => {}
            }
        }
        order
            .into_iter()
            .filter_map(|id| views.remove(&id))
            .collect()
    }

    /// Usage reports: the `_meta.nori` object of each usage update.
    pub fn usage_reports(&self) -> Vec<serde_json::Value> {
        self.updates()
            .filter_map(|update| match update {
                acp::SessionUpdate::UsageUpdate(usage) => usage
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("nori"))
                    .cloned(),
                acp::SessionUpdate::SessionInfoUpdate(info) => info
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("nori"))
                    .filter(|nori| nori.get("status").is_none())
                    .cloned(),
                _ => None,
            })
            .collect()
    }
}

fn blank(id: &str) -> ToolView {
    ToolView {
        id: id.to_owned(),
        title: String::new(),
        kind: acp::ToolKind::Other,
        status: acp::ToolCallStatus::Pending,
        statuses: Vec::new(),
        locations: Vec::new(),
        raw_input: None,
        raw_output: None,
        text: String::new(),
        diffs: Vec::new(),
        updated_before_announced: false,
    }
}

fn fold_content(view: &mut ToolView, content: &[acp::ToolCallContent]) {
    view.text.clear();
    view.diffs.clear();
    for item in content {
        match item {
            acp::ToolCallContent::Content(content) => {
                if let acp::ContentBlock::Text(text) = &content.content {
                    view.text.push_str(&text.text);
                }
            }
            acp::ToolCallContent::Diff(diff) => view.diffs.push((
                diff.path.display().to_string(),
                diff.old_text.clone(),
                diff.new_text.clone(),
            )),
            _ => {}
        }
    }
}
