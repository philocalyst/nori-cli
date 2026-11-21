use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::acp;
use crate::acp::v1::RequestId;
use crate::monitor::MonitorWire;
use crate::route::AttemptSuperseded;
use crate::route::RetryScheduled;
use crate::route::RouteChanged;
use crate::subagent::SubagentWire;

/// An event emitted by one Nori harness session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "source", content = "event", rename_all = "snake_case")]
// Keep the approved schema-shaped API unboxed; consumers should not inherit a
// Nori-specific indirection solely because ACP's response aggregate is large.
#[allow(clippy::large_enum_variant)]
pub enum SessionEvent {
    /// Semantics owned by the Agent Client Protocol.
    Acp(AcpEvent),
    /// Harness behavior that ACP does not define.
    Nori(NoriEvent),
}

/// Raw ACP traffic emitted by the agent toward its client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "message_type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum AcpEvent {
    Notification(acp::v1::AgentNotification),
    /// A `session/update` for a subagent's session, not the root's. It is raw ACP traffic like
    /// any other notification, but kept apart so that nothing mistakes a child's text, tool
    /// calls, idle state, or usage for the root's.
    ChildNotification(acp::v1::SessionNotification),
    Request {
        request_id: RequestId,
        request: acp::v1::AgentRequest,
    },
    Response {
        request_id: RequestId,
        response: Result<acp::v1::AgentResponse, acp::v1::Error>,
    },
}

/// Notifications owned by the Nori harness rather than ACP.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event_type", content = "event", rename_all = "snake_case")]
pub enum NoriEvent {
    SessionStarted(SessionStarted),
    /// Initial replay has finished for this agent session, including when there was no replay.
    SessionReady(acp::v1::SessionId),
    /// Canonical inclusive record range for a completed assistant response.
    ConversationResponseBoundary(ConversationResponseBoundary),
    SessionPhaseChanged(SessionPhase),
    SessionEnded(SessionEnded),
    QueueChanged(QueueSnapshot),
    /// The queued user prompts for one child, keyed by its public session ID.
    SubagentQueueChanged {
        session_id: acp::v1::SessionId,
        prompts: Vec<String>,
    },
    ReplayStarted(ReplayStarted),
    ReplayFinished,
    ContextCompacted(ContextCompactedEvent),
    SessionForked(SessionForked),
    GoalChanged(Option<ThreadGoal>),
    CapabilitiesChanged(NoriCapabilities),
    Undo(UndoEvent),
    UserShell(UserShellEvent),
    HookOutput(HookOutput),
    PromptSummaryUpdated(PromptSummary),
    Notice(Notice),
    RequestFailed(RequestFailure),
    /// The session moved to another route after a failure; it now runs on `to`.
    RouteChanged(RouteChanged),
    /// A failed request will be retried on the same route after a wait.
    RetryScheduled(RetryScheduled),
    /// The failed attempt's output so far is obsolete; the request starts over.
    AttemptSuperseded(AttemptSuperseded),
    /// Journals written before routes record an exhausted provider this way. Never emitted
    /// now; kept so older transcripts still load.
    SubscriptionExhausted(SubscriptionExhausted),
    /// A subagent session was announced, changed state, or exchanged a message.
    Subagent(SubagentWire),
    /// The results of tasks the session delegated were handed to its agent in a turn of
    /// their own.
    TaskResultsDelivered(TaskResultsDelivered),
    /// The monitors the session's agent armed: how many watch, and what they reported to it.
    Monitor(MonitorWire),
    /// A request the client was asked to answer no longer needs an answer, such as a delegated
    /// task's permission request after the task ended.
    RequestWithdrawn(RequestId),
    /// The agent is asking for permission in a turn that outlived the request that started it:
    /// it kept working after its turn ended, or after the user cancelled and it ignored the
    /// cancel. The permission request itself follows as an ordinary request; this says what
    /// the user is being asked to decide. When `after_cancel` is set the user may also stop the
    /// agent (`stop_agent`) instead of allowing or denying.
    AgentContinued(AgentContinued),
    /// Permission requests answered as cancelled because the user cancelled their turn. One
    /// event per tool per window: `count` grows while the agent keeps asking.
    RequestDeclined(RequestDeclined),
    /// Only in a replay: the journal ends on a prompt the agent never answered, because the
    /// session ended (crash, quit) before it did. The prompt is offered back; it was not
    /// lost and was not answered.
    PromptUnanswered(PromptUnanswered),
    /// Prompts typed ahead in a session that ended before they started, restored when it was
    /// resumed. They are not running and nothing starts them by itself: the user sends the
    /// oldest (promotes it) or drops one by its id. Published whenever that list changes, and
    /// empty when the last one is gone.
    QueueRestored(QueueRestored),
    /// A prompt is outstanding and the agent has done nothing at all for `silent_for_secs`: no
    /// text, tool update, request, or session fact. This is not a failure and nothing is
    /// cancelled: a long silent tool and a lost response look the same from here. It says what
    /// the user needs to decide, and Esc cancels through the normal path. Republished as the
    /// silence grows; cleared by [`NoriEvent::AgentHeard`].
    AgentSilent(AgentSilent),
    /// The agent that was silent did something. Any activity clears the silence.
    AgentHeard,
    /// A plugin's `session.move` asks the frontend that owns the terminal to move this session to
    /// `machine`. The harness cannot: the handover (a Rex pane, or `ssh -t` in this terminal) is
    /// the frontend's, so it runs its own move flow and the person sees that flow's lines.
    SessionMoveRequested {
        machine: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSilent {
    pub silent_for_secs: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueRestored {
    pub prompts: Vec<RestoredPrompt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredPrompt {
    /// Names the prompt to drop it.
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptUnanswered {
    /// What the user had typed.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentContinued {
    /// The agent's display name.
    pub agent: String,
    /// The permission request the user is about to be asked to answer.
    pub request_id: RequestId,
    /// What the agent wants to do, as the agent titled it.
    pub tool: String,
    pub cause: AgentTurnCause,
    /// The user cancelled and the agent ignored it.
    pub after_cancel: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestDeclined {
    pub tool: String,
    pub count: i64,
    /// Which child the declined requests came from, when they were relayed from one.
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Legacy journal record; see [`NoriEvent::SubscriptionExhausted`].
pub struct SubscriptionExhausted {
    pub request_id: RequestId,
    pub echo_id: Option<String>,
    pub models: Vec<String>,
    pub ranked: bool,
    pub content: Vec<acp::v1::ContentBlock>,
    pub context: Option<String>,
    pub original_error: String,
}

/// Results of delegated tasks, handed to the agent that delegated them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskResultsDelivered {
    pub tasks: Vec<DeliveredTask>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredTask {
    pub task_id: String,
    pub title: String,
    /// The display name of the agent the task ran on.
    pub agent: String,
    /// What the actor that ran the task is called (its alias), when it has one.
    #[serde(default)]
    pub name: Option<String>,
}

/// Record identities in the source transcript, not presentation-cell indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationResponseBoundary {
    pub transcript_id: String,
    pub start: i64,
    pub end: i64,
}

/// Persistent and agent-side identities established for a harness session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStarted {
    pub transcript_id: Option<String>,
    pub acp_session_id: acp::v1::SessionId,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub history_log_id: i64,
    pub history_entry_count: i64,
    /// The session configuration the agent advertised when the session was
    /// created, so clients can show the agent's model, mode, and other options
    /// from the first frame instead of waiting for an update.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_options: Vec<acp::v1::SessionConfigOption>,
    /// Set when the harness replaced the session's agent in the middle of the conversation:
    /// this session continues it, so a front end updates its header and status in place
    /// instead of starting a new conversation card. The `RouteChanged` that precedes it is the
    /// one thing to show.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap: Option<nori_workflow::RouteCause>,
}

/// Why the agent is working without a request of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentTurnCause {
    /// The agent kept working after the local request that started it ended.
    Detached,
    /// The agent declared a turn through Nori status metadata, for example a turn another
    /// frontend started.
    Observed,
    /// The agent produced activity with no request and no status hint.
    Unprompted,
}

/// The harness-owned phase of the session's single turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionPhase {
    Idle,
    Loading {
        request_id: RequestId,
    },
    Prompting {
        request_id: RequestId,
    },
    Cancelling {
        request_id: RequestId,
    },
    /// The agent is working without a local request. `cancelling` means `session/cancel` has
    /// been sent and the agent has not gone quiet yet.
    Agent {
        cause: AgentTurnCause,
        #[serde(default)]
        cancelling: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionEnded {
    pub reason: SessionEndReason,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEndReason {
    Shutdown,
    Closed,
    ConnectionLost,
    SpawnFailed,
    TimedOut,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSnapshot {
    pub prompts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayStarted {
    pub source: ReplaySource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySource {
    Transcript,
    Agent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextCompactedEvent {
    pub summary: Option<String>,
}

/// Emitted when branch-at-head `/fork` forks the transcript: the previous
/// conversation is frozen and a fresh conversation is seeded from it and made
/// active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionForked {
    pub previous_conversation_id: String,
    pub new_conversation_id: String,
    pub new_acp_session_id: acp::v1::SessionId,
}

/// Capabilities implemented by Nori around the ACP agent.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NoriCapabilities {
    pub goal_management: bool,
    pub builtin_commands: HashMap<String, CommandAvailability>,
}

/// Availability of a harness-owned command in the active session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAvailability {
    pub enabled: bool,
    pub reason: Option<String>,
}

/// Persistent goal state owned by the Nori harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadGoal {
    pub objective: String,
    pub status: ThreadGoalStatus,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadGoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UndoEvent {
    Started {
        operation_id: String,
        message: Option<String>,
    },
    Completed {
        operation_id: String,
        success: bool,
        message: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum UserShellEvent {
    Started {
        operation_id: String,
        command: String,
        cwd: PathBuf,
    },
    Output {
        operation_id: String,
        stream: UserShellStream,
        chunk: Vec<u8>,
    },
    Finished {
        operation_id: String,
        exit_code: i32,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserShellStream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookOutput {
    pub message: String,
    pub level: HookOutputLevel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookOutputLevel {
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptSummary {
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestFailure {
    pub request_id: Option<RequestId>,
    pub message: String,
    pub kind: RequestFailureKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestFailureKind {
    Retryable,
    Fatal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn child_queue_is_scoped_and_root_queue_remains_compatible() {
        let child = NoriEvent::SubagentQueueChanged {
            session_id: acp::v1::SessionId::new("child"),
            prompts: vec!["first".into(), "second".into()],
        };
        let encoded = json!({"event_type":"subagent_queue_changed", "event":{"session_id":"child", "prompts":["first","second"]}});
        assert_eq!(serde_json::to_value(&child).unwrap(), encoded);
        assert_eq!(serde_json::from_value::<NoriEvent>(encoded).unwrap(), child);
        let previous = json!({"event_type":"queue_changed", "event":{"prompts":["root"]}});
        assert_eq!(
            serde_json::from_value::<NoriEvent>(previous).unwrap(),
            NoriEvent::QueueChanged(QueueSnapshot {
                prompts: vec!["root".into()]
            })
        );
    }
}
