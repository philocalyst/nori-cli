use super::*;
use crate::terminal_status::Report;
use crate::terminal_status::State;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1::AgentRequest;
use pretty_assertions::assert_eq;

fn report(status: &ProgramStatus, path: &str) -> Report {
    status.nodes[path].report()
}

fn subagent(parent: &str, child: &str, title: Patch<String>, state: SubagentState) -> SessionEvent {
    SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(SubagentUpdate {
        parent_session_id: SessionId::new(parent),
        session_id: SessionId::new(child),
        agent: Patch::Keep,
        model: Patch::Keep,
        name: Patch::Keep,
        title,
        description: Patch::Keep,
        state: Patch::Set(state),
    })))
}

#[test]
fn root_and_nested_children_have_independent_lifecycles() {
    let mut status = ProgramStatus::default();
    status.root = Some("root".into());
    status.idle();
    for (parent, child) in [("root", "child"), ("child", "grandchild")] {
        status.event(&subagent(
            parent,
            child,
            Patch::Set(child.into()),
            SubagentState::Running,
        ));
    }
    assert_eq!(report(&status, "").state, State::Idle);
    assert_eq!(report(&status, "s1").state, State::Working);
    assert_eq!(report(&status, "s1/s2").state, State::Working);
    assert_eq!(
        report(&status, "s1/s2").title.as_deref(),
        Some("grandchild")
    );
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
    assert_eq!(report(&status, "s1/s2").kind, Some(Kind::Question));
    assert_eq!(report(&status, "s1/s2").msg.as_deref(), Some("Which city?"));
    assert_eq!(report(&status, "s1/s2").state, State::Blocked);
    assert_eq!(report(&status, "").state, State::Idle);
    status.event(&SessionEvent::Nori(NoriEvent::RequestWithdrawn(
        question_id,
    )));
    assert_eq!(report(&status, "s1/s2").state, State::Working);
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
    assert_eq!(report(&status, "s1/s2").kind, Some(Kind::Permission));
    status.event(&subagent(
        "child",
        "grandchild",
        Patch::Keep,
        SubagentState::Running,
    ));
    assert_eq!(report(&status, "s1/s2").kind, Some(Kind::Permission));
    assert_eq!(report(&status, "").state, State::Idle);
    status.answered(&request_id);
    assert_eq!(report(&status, "s1/s2").state, State::Working);
    status.auth(Some(&SessionId::new("grandchild")));
    assert_eq!(report(&status, "s1/s2").kind, Some(Kind::Auth));
    assert_eq!(report(&status, "").state, State::Idle);
    status.finish_auth();
    assert_eq!(report(&status, "s1/s2").state, State::Working);
    status.event(&subagent(
        "root",
        "child",
        Patch::Keep,
        SubagentState::Ended(SubagentOutcome::Closed),
    ));
    assert_eq!(status.nodes.len(), 1);
    assert!(status.children.is_empty());
    status.auth(None);
    assert_eq!(report(&status, "").kind, Some(Kind::Auth));
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
    assert_eq!(report(&status, "").kind, Some(Kind::Auth));
    status.finish_auth();
    assert_eq!(report(&status, "").state, State::Idle);
    status.clear();
    assert!(status.nodes.is_empty());
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
        status.event(&subagent(
            &parent,
            &format!("child{index}"),
            Patch::Keep,
            SubagentState::Running,
        ));
    }
    assert_eq!(status.children.len(), 8);
    assert_eq!(status.nodes.len(), 9);
    assert!(status.children.values().all(|path| path.len() <= 128));
}
