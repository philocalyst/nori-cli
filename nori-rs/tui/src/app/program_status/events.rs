//! The session's events, as they change what the terminal should show.

use std::time::Duration;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEndReason;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::SubagentWire;
use nori_protocol::acp::MaybeUndefined;
use nori_protocol::acp::v1::AgentNotification;
use nori_protocol::acp::v1::AgentRequest;
use nori_protocol::acp::v1::AgentResponse;
use nori_protocol::acp::v1::PlanEntryStatus;
use nori_protocol::acp::v1::RequestId;
use nori_protocol::acp::v1::SessionNotification;
use nori_protocol::acp::v1::SessionUpdate;
use nori_protocol::acp::v1::StopReason;
use nori_protocol::acp::v1::ToolCallStatus;

use super::MAX_CHILDREN;
use super::MAX_DEPTH;
use super::MAX_ID_BYTES;
use super::ProgramStatus;
use super::node::Block;
use super::node::BlockKey;
use super::node::Node;
use super::node::Phase;
use crate::status_indicator_widget::fmt_elapsed_compact;
use crate::terminal_status::Kind;

impl ProgramStatus {
    pub(in crate::app) fn event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Nori(NoriEvent::ReplayStarted(_)) => self.replaying = true,
            SessionEvent::Nori(NoriEvent::ReplayFinished) => self.replaying = false,
            SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                let auth_pending = self.nodes.get("").is_some_and(|root| {
                    root.blocks.iter().any(|block| block.key == BlockKey::Auth)
                });
                self.clear();
                self.root = Some(started.acp_session_id.clone());
                self.session_title = None;
                self.prompt_summary = None;
                self.project = started
                    .cwd
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned());
                self.idle();
                if auth_pending {
                    self.auth(None);
                }
            }
            SessionEvent::Nori(NoriEvent::PromptSummaryUpdated(summary)) => {
                self.prompt_summary = Some(summary.summary.clone());
                self.refresh_root_title();
            }
            SessionEvent::Acp(AcpEvent::Notification(AgentNotification::SessionNotification(
                notification,
            ))) if matches!(notification.update, SessionUpdate::SessionInfoUpdate(_)) => {
                // The session's title is a fact about it, replayed or not.
                if self.root.as_ref() == Some(&notification.session_id)
                    && let SessionUpdate::SessionInfoUpdate(info) = &notification.update
                {
                    match &info.title {
                        MaybeUndefined::Value(title) => self.session_title = Some(title.clone()),
                        MaybeUndefined::Null => self.session_title = None,
                        MaybeUndefined::Undefined => {}
                    }
                    self.refresh_root_title();
                }
            }
            _ if self.replaying => {}
            SessionEvent::Acp(AcpEvent::Notification(AgentNotification::SessionNotification(
                notification,
            )))
            | SessionEvent::Acp(AcpEvent::ChildNotification(notification)) => {
                self.notification(notification);
            }
            SessionEvent::Nori(NoriEvent::SessionPhaseChanged(phase)) => self.phase_changed(phase),
            SessionEvent::Acp(AcpEvent::Response {
                response: Ok(AgentResponse::PromptResponse(response)),
                ..
            }) => {
                let Some(root) = self.nodes.get_mut("") else {
                    return;
                };
                match response.stop_reason {
                    StopReason::Cancelled => root.rest(),
                    StopReason::Refusal => root.fail(Some("The agent declined to continue".into())),
                    StopReason::MaxTokens => {
                        root.finish(Some("Stopped at the token limit".into()));
                    }
                    StopReason::MaxTurnRequests => {
                        root.finish(Some("Stopped at the request limit".into()));
                    }
                    // `EndTurn`, and any reason a later protocol adds: the turn is over.
                    _ => root.finish(None),
                }
                self.sync("");
            }
            SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                if let Some(root) = self.nodes.get_mut("") {
                    root.fail(Some(failure.message.clone()));
                    self.sync("");
                }
            }
            SessionEvent::Nori(NoriEvent::RetryScheduled(retry)) => {
                let wait = Duration::from_millis(u64::try_from(retry.wait_ms).unwrap_or_default());
                if let Some(root) = self.nodes.get_mut("") {
                    root.begin_turn();
                    root.set_activity(format!(
                        "Retrying in {} ({}/{})",
                        fmt_elapsed_compact(wait.as_secs().max(1)),
                        retry.attempt,
                        retry.max_attempts
                    ));
                    self.sync("");
                }
            }
            SessionEvent::Nori(NoriEvent::AgentSilent(silent)) => {
                if let Some(root) = self.nodes.get_mut("") {
                    root.set_activity(format!(
                        "No activity for {}",
                        fmt_elapsed_compact(
                            u64::try_from(silent.silent_for_secs).unwrap_or_default()
                        )
                    ));
                    self.sync("");
                }
            }
            SessionEvent::Nori(NoriEvent::AgentHeard) => {
                if let Some(root) = self.nodes.get_mut("")
                    && root.phase == Phase::Working
                {
                    root.activity = None;
                    self.sync("");
                }
            }
            SessionEvent::Nori(NoriEvent::SessionEnded(ended)) => {
                self.clear();
                self.idle();
                match ended.reason {
                    SessionEndReason::Shutdown | SessionEndReason::Closed => {}
                    SessionEndReason::ConnectionLost
                    | SessionEndReason::SpawnFailed
                    | SessionEndReason::TimedOut => {
                        if let Some(root) = self.nodes.get_mut("") {
                            root.fail(ended.message.clone());
                            self.sync("");
                        }
                    }
                }
            }
            SessionEvent::Nori(NoriEvent::Undo(nori_protocol::UndoEvent::Completed {
                success: true,
                ..
            })) => {
                let top: Vec<String> = self
                    .nodes
                    .keys()
                    .filter(|path| !path.is_empty() && !path.contains('/'))
                    .cloned()
                    .collect();
                for path in top {
                    self.drop_subtree(&path);
                }
                if let Some(root) = self.nodes.get_mut("") {
                    root.rest();
                    self.sync("");
                }
            }
            SessionEvent::Nori(NoriEvent::RequestWithdrawn(id)) => self.answered(id),
            SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: AgentRequest::ExtMethodRequest(request),
            }) if request.method.as_ref() == "_nori/request_user_input" => {
                self.question(request_id, request.params.get());
            }
            SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: AgentRequest::RequestPermissionRequest(request),
            }) => {
                let msg = request.tool_call.fields.title.as_ref().map_or_else(
                    || "Permission required".to_owned(),
                    |tool| format!("Approve {tool}?"),
                );
                self.block(&request.session_id, request_id, Kind::Permission, msg);
            }
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
                self.subagent(update);
            }
            _ => {}
        }
    }

    /// The root's turn started, rested, or is about to.
    fn phase_changed(&mut self, phase: &SessionPhase) {
        let Some(root) = self.nodes.get_mut("") else {
            return;
        };
        match phase {
            SessionPhase::Loading { .. }
            | SessionPhase::Prompting { .. }
            | SessionPhase::Agent {
                cancelling: false, ..
            } => {
                if root.phase != Phase::Working {
                    root.begin_turn();
                    // What the last turn left for the user is old news once another begins.
                    self.drop_finished_children();
                }
            }
            SessionPhase::Cancelling { .. }
            | SessionPhase::Agent {
                cancelling: true, ..
            } => root.rest(),
            SessionPhase::Idle => {
                if root.phase == Phase::Working {
                    root.rest();
                }
            }
        }
        self.sync("");
    }

    /// Activity inside a session: the tool it runs, its plan, its title.
    fn notification(&mut self, notification: &SessionNotification) {
        let Some(path) = self.path_of(&notification.session_id) else {
            return;
        };
        let Some(node) = self.nodes.get_mut(&path) else {
            return;
        };
        match &notification.update {
            SessionUpdate::ToolCall(call) => {
                node.turn.tools.insert(call.tool_call_id.clone());
                node.current_tool = Some(call.tool_call_id.clone());
                node.set_activity(call.title.clone());
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if node.current_tool.as_ref() == Some(&update.tool_call_id)
                    && let Some(title) = &update.fields.title
                {
                    node.set_activity(title.clone());
                }
                if matches!(
                    update.fields.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                ) && node.current_tool.as_ref() == Some(&update.tool_call_id)
                {
                    node.set_activity("Thinking");
                }
            }
            SessionUpdate::AgentThoughtChunk(_) => node.set_activity("Thinking"),
            SessionUpdate::AgentMessageChunk(_) => node.set_activity("Writing a response"),
            SessionUpdate::Plan(plan) => {
                let total = i64::try_from(plan.entries.len()).unwrap_or(i64::MAX);
                let completed = i64::try_from(
                    plan.entries
                        .iter()
                        .filter(|entry| entry.status == PlanEntryStatus::Completed)
                        .count(),
                )
                .unwrap_or_default();
                node.progress = (total > 0).then(|| completed * 100 / total);
            }
            _ => return,
        }
        self.sync(&path);
    }

    /// A request on `session` waits for the user.
    fn block(
        &mut self,
        session: &nori_protocol::acp::v1::SessionId,
        id: &RequestId,
        kind: Kind,
        msg: String,
    ) {
        let Some(path) = self.path_of(session) else {
            return;
        };
        let Some(node) = self.nodes.get_mut(&path) else {
            return;
        };
        node.add_block(Block {
            key: BlockKey::Request(id.clone()),
            kind,
            msg,
        });
        self.requests.insert(id.clone(), path.clone());
        self.sync(&path);
    }

    /// An agent asks the user a question. A question that asks for a secret is a credential.
    fn question(&mut self, id: &RequestId, params: &str) {
        let Ok(params) = serde_json::from_str::<serde_json::Value>(params) else {
            return;
        };
        let Some(session) = params["sessionId"].as_str() else {
            return;
        };
        let first = &params["questions"][0];
        let kind = if first["isSecret"].as_bool() == Some(true) {
            Kind::Auth
        } else {
            Kind::Question
        };
        let msg = ["question", "header"]
            .iter()
            .find_map(|key| first[*key].as_str().filter(|text| !text.trim().is_empty()))
            .unwrap_or("Answer requested")
            .to_owned();
        self.block(
            &nori_protocol::acp::v1::SessionId::new(session),
            id,
            kind,
            msg,
        );
    }

    /// A subagent appeared, changed, or ended.
    fn subagent(&mut self, update: &SubagentUpdate) {
        if matches!(
            update.state,
            Patch::Set(SubagentState::Ended(SubagentOutcome::Closed))
        ) {
            if let Some(path) = self.children.get(&update.session_id).cloned() {
                self.drop_subtree(&path);
            }
            return;
        }
        if !self.children.contains_key(&update.session_id) && !self.adopt(update) {
            return;
        }
        let Some(path) = self.children.get(&update.session_id).cloned() else {
            return;
        };
        let Some(node) = self.nodes.get_mut(&path) else {
            return;
        };
        let mut title = None;
        let mut description = None;
        update.title.clone().apply_to(&mut title);
        update.description.clone().apply_to(&mut description);
        // What Nori calls it: the task, else the type; the type is the detail when it adds one.
        match (&description, &title) {
            (Some(task), Some(kind)) if task != kind => {
                node.title = Some(task.clone());
                node.detail = Some(kind.clone());
            }
            (Some(label), _) | (None, Some(label)) => node.title = Some(label.clone()),
            (None, None) => {}
        }
        match &update.state {
            Patch::Set(SubagentState::Running) => node.begin_turn(),
            Patch::Set(
                SubagentState::Idle { .. } | SubagentState::Ended(SubagentOutcome::Cancelled),
            ) => {
                node.rest();
            }
            Patch::Set(SubagentState::Ended(SubagentOutcome::Completed { .. })) => {
                node.finish(None)
            }
            Patch::Set(SubagentState::Ended(SubagentOutcome::Failed { reason })) => {
                node.fail(Some(reason.clone().unwrap_or_else(|| "Failed".to_owned())));
            }
            Patch::Set(SubagentState::Ended(SubagentOutcome::Closed))
            | Patch::Keep
            | Patch::Clear => {}
        }
        self.sync(&path);
    }

    /// Give a newly announced subagent its record, under its parent's. A subagent the protocol
    /// has no room for, or whose parent is unknown, gets none.
    fn adopt(&mut self, update: &SubagentUpdate) -> bool {
        if self.children.len() >= MAX_CHILDREN {
            return false;
        }
        let Some(parent) = self.path_of(&update.parent_session_id) else {
            return false;
        };
        self.next_child += 1;
        let segment = format!("s{}", self.next_child);
        let path = if parent.is_empty() {
            segment
        } else {
            format!("{parent}/{segment}")
        };
        if path.len() > MAX_ID_BYTES || path.split('/').count() > MAX_DEPTH {
            return false;
        }
        if let Some(parent) = self.nodes.get_mut(&parent) {
            parent.turn.children += 1;
        }
        self.children
            .insert(update.session_id.clone(), path.clone());
        self.nodes.insert(path.clone(), Node::new(&path));
        true
    }
}
