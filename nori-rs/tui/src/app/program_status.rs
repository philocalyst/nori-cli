//! Program Status Protocol (OSC 7501) for the active ACP conversation.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1::AgentRequest;
use nori_protocol::acp::v1::AgentResponse;
use nori_protocol::acp::v1::RequestId;
use nori_protocol::acp::v1::SessionId;
use std::collections::HashMap;
use std::io::IsTerminal;
use std::io::Write;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    state: &'static str,
    kind: Option<&'static str>,
    title: Option<String>,
    msg: Option<String>,
}
impl Record {
    fn new(state: &'static str) -> Self {
        Self {
            state,
            kind: None,
            title: None,
            msg: None,
        }
    }
    fn sequence(&self, id: &str) -> String {
        let mut body = format!("state={}:app=nori", self.state);
        if !id.is_empty() {
            body.push_str(&format!(":id={id}"));
        }
        if let Some(kind) = self.kind {
            body.push_str(&format!(":kind={kind}"));
        }
        for (key, value, limit) in [("title", &self.title, 192), ("msg", &self.msg, 2048)] {
            if let Some(value) = value {
                let mut clean = String::new();
                for c in value.chars().filter(|c| !c.is_control()) {
                    if clean.len() + c.len_utf8() > limit {
                        break;
                    }
                    clean.push(c);
                }
                body.push_str(&format!(":{key}={}", STANDARD.encode(clean)));
            }
        }
        format!("\x1b]7501;{body}\x1b\\")
    }
}
#[derive(Default)]
struct Child {
    path: String,
    title: Option<String>,
    description: Option<String>,
}
#[derive(Default)]
pub(super) struct ProgramStatus {
    root: Option<SessionId>,
    children: HashMap<SessionId, Child>,
    records: HashMap<String, Record>,
    requests: HashMap<RequestId, String>,
    next_id: i64,
    replaying: bool,
    before_auth: Option<(String, Record)>,
}
impl ProgramStatus {
    fn write(sequence: &str) {
        if std::io::stdout().is_terminal() {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(sequence.as_bytes());
            let _ = out.flush();
        }
    }
    fn publish(&mut self, id: &str, record: Record) {
        let sequence = record.sequence(id);
        if self
            .records
            .get(id)
            .is_some_and(|previous| previous.sequence(id) == sequence)
        {
            return;
        }
        Self::write(&sequence);
        self.records.insert(id.to_owned(), record);
    }
    pub(super) fn clear(&mut self) {
        if !self.records.is_empty() {
            Self::write("\x1b]7501;state=clear\x1b\\");
        }
        self.records.clear();
        self.children.clear();
        self.requests.clear();
        self.root = None;
        self.before_auth = None;
        self.replaying = false;
    }
    fn clear_children(&mut self) {
        for path in self
            .children
            .values()
            .map(|child| &child.path)
            .filter(|path| !path.contains('/'))
        {
            Self::write(&format!("\x1b]7501;state=clear:id={path}\x1b\\"));
        }
        self.records.retain(|path, _| path.is_empty());
        self.children.clear();
        self.requests.retain(|_, path| path.is_empty());
    }
    pub(super) fn idle(&mut self) {
        self.publish("", Record::new("idle"));
    }
    pub(super) fn acknowledge(&mut self) {
        if matches!(
            self.records.get("").map(|r| r.state),
            Some("done" | "error")
        ) {
            self.idle();
        }
    }
    pub(super) fn auth(&mut self, session: Option<&SessionId>) {
        let path = match session {
            Some(session) => match self.children.get(session) {
                Some(child) => child.path.clone(),
                None => return,
            },
            None => String::new(),
        };
        if self.before_auth.is_none() {
            self.before_auth = Some((
                path.clone(),
                self.records
                    .get(&path)
                    .cloned()
                    .unwrap_or_else(|| Record::new("idle")),
            ));
        }
        let mut r = Record::new("blocked");
        r.kind = Some("auth");
        r.title = self
            .records
            .get(&path)
            .and_then(|record| record.title.clone());
        r.msg = Some("Sign in to continue".into());
        self.publish(&path, r);
    }
    pub(super) fn finish_auth(&mut self) {
        if let Some((path, record)) = self.before_auth.take()
            && self
                .records
                .get(&path)
                .is_some_and(|r| r.kind == Some("auth"))
        {
            self.publish(&path, record);
        }
    }
    pub(super) fn answered(&mut self, id: &RequestId) {
        if let Some(path) = self.requests.remove(id)
            && !self.requests.values().any(|pending| pending == &path)
        {
            let mut r = Record::new("working");
            r.title = self.records.get(&path).and_then(|r| r.title.clone());
            self.publish(&path, r);
        }
    }
    fn tool_update(&mut self, notification: &nori_protocol::acp::v1::SessionNotification) {
        use nori_protocol::acp::v1::SessionUpdate;
        let title = match &notification.update {
            SessionUpdate::ToolCall(call) => Some(&call.title),
            SessionUpdate::ToolCallUpdate(update) => update.fields.title.as_ref(),
            _ => None,
        };
        let path = if self.root.as_ref() == Some(&notification.session_id) {
            Some(String::new())
        } else {
            self.children
                .get(&notification.session_id)
                .map(|child| child.path.clone())
        };
        if let (Some(title), Some(path)) = (title, path)
            && let Some(mut record) = self.records.get(&path).cloned()
            && record.state == "working"
        {
            record.msg = Some(title.clone());
            self.publish(&path, record);
        }
    }
    pub(super) fn event(&mut self, event: &SessionEvent) {
        match event {
            SessionEvent::Nori(NoriEvent::ReplayStarted(_)) => self.replaying = true,
            SessionEvent::Nori(NoriEvent::ReplayFinished) => self.replaying = false,
            SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                let root_auth_pending = self
                    .before_auth
                    .as_ref()
                    .is_some_and(|(path, _)| path.is_empty());
                self.clear();
                self.root = Some(started.acp_session_id.clone());
                self.idle();
                if root_auth_pending {
                    self.auth(None);
                }
            }
            _ if self.replaying => {}
            SessionEvent::Acp(AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
            ))
            | SessionEvent::Acp(AcpEvent::ChildNotification(notification)) => {
                self.tool_update(notification);
            }
            SessionEvent::Nori(NoriEvent::SessionPhaseChanged(phase)) => match phase {
                SessionPhase::Loading { .. }
                | SessionPhase::Prompting { .. }
                | SessionPhase::Agent {
                    cancelling: false, ..
                } => {
                    if !self.requests.values().any(String::is_empty) {
                        self.publish("", Record::new("working"));
                    }
                }
                SessionPhase::Cancelling { .. }
                | SessionPhase::Agent {
                    cancelling: true, ..
                } => {
                    self.idle();
                }
                SessionPhase::Idle => {
                    if !matches!(
                        self.records.get("").map(|r| r.state),
                        Some("done" | "error" | "blocked")
                    ) {
                        self.idle();
                    }
                }
            },
            SessionEvent::Acp(AcpEvent::Response {
                response: Ok(AgentResponse::PromptResponse(response)),
                ..
            }) => {
                let state = if matches!(
                    response.stop_reason,
                    nori_protocol::acp::v1::StopReason::Cancelled
                ) {
                    "idle"
                } else {
                    "done"
                };
                self.publish("", Record::new(state));
            }
            SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                let mut r = Record::new("error");
                r.msg = Some(failure.message.clone());
                self.publish("", r);
            }
            SessionEvent::Nori(NoriEvent::SessionEnded(ended)) => {
                self.clear();
                match ended.reason {
                    nori_protocol::SessionEndReason::Shutdown
                    | nori_protocol::SessionEndReason::Closed => self.idle(),
                    nori_protocol::SessionEndReason::ConnectionLost
                    | nori_protocol::SessionEndReason::SpawnFailed
                    | nori_protocol::SessionEndReason::TimedOut => {
                        let mut record = Record::new("error");
                        record.msg = ended.message.clone();
                        self.publish("", record);
                    }
                }
            }
            SessionEvent::Nori(NoriEvent::Undo(nori_protocol::UndoEvent::Completed {
                success: true,
                ..
            })) => {
                self.clear_children();
                self.idle();
            }
            SessionEvent::Nori(NoriEvent::RequestWithdrawn(id)) => self.answered(id),
            SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: AgentRequest::ExtMethodRequest(request),
            }) if request.method.as_ref() == "_nori/request_user_input" => {
                if let Ok(params) = serde_json::from_str::<serde_json::Value>(request.params.get())
                    && let Some(session) = params["sessionId"].as_str()
                {
                    let session = SessionId::new(session);
                    let path = if self.root.as_ref() == Some(&session) {
                        Some(String::new())
                    } else {
                        self.children.get(&session).map(|child| child.path.clone())
                    };
                    if let Some(path) = path {
                        let mut record = Record::new("blocked");
                        record.kind = Some("question");
                        record.title = self
                            .records
                            .get(&path)
                            .and_then(|record| record.title.clone());
                        record.msg = Some("Answer requested".to_owned());
                        self.requests.insert(request_id.clone(), path.clone());
                        self.publish(&path, record);
                    }
                }
            }
            SessionEvent::Acp(AcpEvent::Request {
                request_id,
                request: AgentRequest::RequestPermissionRequest(request),
            }) => {
                let path = if self.root.as_ref() == Some(&request.session_id) {
                    Some(String::new())
                } else {
                    self.children
                        .get(&request.session_id)
                        .map(|c| c.path.clone())
                };
                if let Some(path) = path {
                    let mut r = Record::new("blocked");
                    r.kind = Some("permission");
                    r.title = self.records.get(&path).and_then(|r| r.title.clone());
                    r.msg = Some(
                        request
                            .tool_call
                            .fields
                            .title
                            .clone()
                            .unwrap_or_else(|| "Permission required".into()),
                    );
                    self.requests.insert(request_id.clone(), path.clone());
                    self.publish(&path, r);
                }
            }
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                if matches!(
                    update.state,
                    Patch::Set(SubagentState::Ended(SubagentOutcome::Closed))
                ) =>
            {
                if let Some(child) = self.children.get(&update.session_id) {
                    let path = child.path.clone();
                    let prefix = format!("{path}/");
                    Self::write(&format!("\x1b]7501;state=clear:id={path}\x1b\\"));
                    self.records
                        .retain(|id, _| id != &path && !id.starts_with(&prefix));
                    self.children
                        .retain(|_, child| child.path != path && !child.path.starts_with(&prefix));
                    self.requests
                        .retain(|_, id| id != &path && !id.starts_with(&prefix));
                }
            }
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
                if !self.children.contains_key(&update.session_id) && self.children.len() < 255 {
                    let parent = if self.root.as_ref() == Some(&update.parent_session_id) {
                        Some(String::new())
                    } else {
                        self.children
                            .get(&update.parent_session_id)
                            .map(|c| c.path.clone())
                    };
                    if let Some(parent) = parent {
                        self.next_id += 1;
                        let segment = format!("s{}", self.next_id);
                        let path = if parent.is_empty() {
                            segment
                        } else {
                            format!("{parent}/{segment}")
                        };
                        if path.len() <= 128 && path.split('/').count() <= 8 {
                            self.children.insert(
                                update.session_id.clone(),
                                Child {
                                    path,
                                    title: None,
                                    description: None,
                                },
                            );
                        }
                    }
                }
                if let Some(child) = self.children.get_mut(&update.session_id) {
                    update.title.clone().apply_to(&mut child.title);
                    update.description.clone().apply_to(&mut child.description);
                    let path = child.path.clone();
                    let title = child.description.clone().or_else(|| child.title.clone());
                    let state = match &update.state {
                        Patch::Set(SubagentState::Running) => Some("working"),
                        Patch::Set(SubagentState::Idle) => Some("idle"),
                        Patch::Set(SubagentState::Ended(SubagentOutcome::Completed { .. })) => {
                            Some("done")
                        }
                        Patch::Set(SubagentState::Ended(SubagentOutcome::Failed { .. })) => {
                            Some("error")
                        }
                        Patch::Set(SubagentState::Ended(SubagentOutcome::Cancelled)) => {
                            Some("idle")
                        }
                        Patch::Set(SubagentState::Ended(SubagentOutcome::Closed)) => None,
                        Patch::Keep | Patch::Clear => None,
                    };
                    let state = if state == Some("working")
                        && self.requests.values().any(|pending| pending == &path)
                    {
                        None
                    } else {
                        state
                    };
                    let previous = self.records.get(&path).cloned();
                    let record = match (state, previous) {
                        (Some(state), Some(previous)) if previous.state == state => Some(previous),
                        (Some(state), _) => Some(Record::new(state)),
                        (None, previous) => previous,
                    };
                    if let Some(mut r) = record {
                        r.title = title;
                        if let Patch::Set(SubagentState::Ended(SubagentOutcome::Failed {
                            reason,
                        })) = &update.state
                        {
                            r.msg = reason.clone();
                        }
                        self.publish(&path, r);
                    }
                }
            }
            _ => {}
        }
    }
}
impl Drop for ProgramStatus {
    fn drop(&mut self) {
        self.clear();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    #[test]
    fn root_and_nested_children_have_independent_lifecycles() {
        use nori_protocol::SubagentUpdate;
        let mut status = ProgramStatus::default();
        status.root = Some("root".into());
        status.idle();
        for (parent, child) in [("root", "child"), ("child", "grandchild")] {
            status.event(&SessionEvent::Nori(NoriEvent::Subagent(
                SubagentWire::Update(SubagentUpdate {
                    parent_session_id: parent.into(),
                    session_id: child.into(),
                    agent: Patch::Keep,
                    model: Patch::Keep,
                    title: Patch::Set(child.into()),
                    description: Patch::Keep,
                    state: Patch::Set(SubagentState::Running),
                }),
            )));
        }
        assert_eq!(status.records[""].state, "idle");
        assert_eq!(status.records["s1"].state, "working");
        assert_eq!(status.records["s1/s2"].state, "working");
        let question_id = RequestId::Str("question".into());
        let question = serde_json::value::to_raw_value(&serde_json::json!({
            "sessionId": "grandchild", "questions": [{"question": "Which city?"}]
        }))
        .unwrap();
        status.event(&SessionEvent::Acp(AcpEvent::Request {
            request_id: question_id.clone(),
            request: AgentRequest::ExtMethodRequest(nori_protocol::acp::v1::ExtRequest::new(
                "_nori/request_user_input",
                std::sync::Arc::from(question),
            )),
        }));
        assert_eq!(status.records["s1/s2"].kind, Some("question"));
        assert_eq!(status.records["s1/s2"].state, "blocked");
        assert_eq!(status.records[""].state, "idle");
        status.event(&SessionEvent::Nori(NoriEvent::RequestWithdrawn(
            question_id,
        )));
        assert_eq!(status.records["s1/s2"].state, "working");
        let request_id = RequestId::Str("permission".into());
        let request = nori_protocol::acp::v1::RequestPermissionRequest::new(
            SessionId::new("grandchild"),
            nori_protocol::acp::v1::ToolCallUpdate::new(
                nori_protocol::acp::v1::ToolCallId::new("tool"),
                nori_protocol::acp::v1::ToolCallUpdateFields::new().title("Run command"),
            ),
            Vec::new(),
        );
        status.event(&SessionEvent::Acp(AcpEvent::Request {
            request_id: request_id.clone(),
            request: AgentRequest::RequestPermissionRequest(request),
        }));
        assert_eq!(status.records["s1/s2"].kind, Some("permission"));
        status.event(&SessionEvent::Nori(NoriEvent::Subagent(
            SubagentWire::Update(SubagentUpdate {
                parent_session_id: "child".into(),
                session_id: "grandchild".into(),
                agent: Patch::Keep,
                model: Patch::Keep,
                title: Patch::Keep,
                description: Patch::Keep,
                state: Patch::Set(SubagentState::Running),
            }),
        )));
        assert_eq!(status.records["s1/s2"].kind, Some("permission"));
        assert_eq!(status.records[""].state, "idle");
        status.answered(&request_id);
        assert_eq!(status.records["s1/s2"].state, "working");
        status.auth(Some(&SessionId::new("grandchild")));
        assert_eq!(status.records["s1/s2"].kind, Some("auth"));
        assert_eq!(status.records[""].state, "idle");
        status.finish_auth();
        assert_eq!(status.records["s1/s2"].state, "working");
        status.event(&SessionEvent::Nori(NoriEvent::Subagent(
            SubagentWire::Update(SubagentUpdate {
                parent_session_id: "root".into(),
                session_id: "child".into(),
                agent: Patch::Keep,
                model: Patch::Keep,
                title: Patch::Keep,
                description: Patch::Keep,
                state: Patch::Set(SubagentState::Ended(SubagentOutcome::Closed)),
            }),
        )));
        assert_eq!(status.records.len(), 1);
        assert!(status.children.is_empty());
        status.auth(None);
        assert_eq!(status.records[""].kind, Some("auth"));
        status.event(&SessionEvent::Nori(NoriEvent::SessionStarted(
            nori_protocol::SessionStarted {
                transcript_id: None,
                acp_session_id: SessionId::new("new-root"),
                cwd: std::path::PathBuf::from("/"),
                transcript_path: None,
                history_log_id: 0,
                history_entry_count: 0,
                config_options: Vec::new(),
                swap: None,
            },
        )));
        assert_eq!(status.records[""].kind, Some("auth"));
        status.finish_auth();
        assert_eq!(status.records[""].state, "idle");
        status.clear();
        assert!(status.records.is_empty());
        assert!(status.children.is_empty());
    }

    #[test]
    fn ignores_child_beyond_protocol_hierarchy_limit() {
        let mut status = ProgramStatus::default();
        status.root = Some("root".into());
        status.idle();
        for index in 0..9 {
            let parent = if index == 0 {
                "root".to_owned()
            } else {
                format!("child{}", index - 1)
            };
            status.event(&SessionEvent::Nori(NoriEvent::Subagent(
                SubagentWire::Update(nori_protocol::SubagentUpdate {
                    parent_session_id: parent.into(),
                    session_id: format!("child{index}").into(),
                    agent: Patch::Keep,
                    model: Patch::Keep,
                    title: Patch::Keep,
                    description: Patch::Keep,
                    state: Patch::Set(SubagentState::Running),
                }),
            )));
        }
        assert_eq!(status.children.len(), 8);
        assert_eq!(status.records.len(), 9);
        assert!(
            status
                .children
                .values()
                .all(|child| child.path.len() <= 128)
        );
    }

    #[test]
    fn encoding_is_single_line_bounded_and_utf8_safe() {
        let mut r = Record::new("blocked");
        r.kind = Some("permission");
        r.title = Some(format!("{}\n\u{80}x", "é".repeat(100)));
        r.msg = Some("a".repeat(3000));
        let sequence = r.sequence("s1/s2");
        assert!(sequence.len() < 4096);
        let body = sequence
            .strip_prefix("\x1b]7501;")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let fields: HashMap<_, _> = body
            .split(':')
            .map(|pair| pair.split_once('=').unwrap())
            .collect();
        assert_eq!(fields["id"], "s1/s2");
        assert_eq!(
            STANDARD.decode(fields["title"]).unwrap(),
            "é".repeat(96).as_bytes()
        );
        assert_eq!(STANDARD.decode(fields["msg"]).unwrap().len(), 2048);
    }
}
