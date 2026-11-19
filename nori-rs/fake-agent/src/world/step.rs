//! The steps of a script: what a fake agent does, one after another.
//!
//! Output, tools, permissions and delegation are what a well-behaved agent does. The faults are
//! what real agents do on their worst day: drop or repeat a response, answer for a stale request,
//! keep working after the turn ended, ask permission after being cancelled, crash. Every fault is
//! a step like any other, so a world can say exactly when it happens.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::tool::PermissionStep;
use super::tool::ToolSpec;
use super::tool::ToolStart;
use super::tool::ToolUpdateStep;
use super::value::Chunking;
use super::value::Span;
use super::value::Text;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    // Output
    /// Stream a chunk of the assistant's answer.
    Say(Say),
    /// Stream reasoning.
    Think(Say),
    /// Let time pass.
    Wait(Wait),
    // Tools
    /// A complete tool call: announce, stream, end.
    Tool(Box<ToolSpec>),
    /// Announce a call and leave it open.
    ToolStart(ToolStart),
    /// Update any call id, announced or not, open or ended.
    ToolUpdate(ToolUpdateStep),
    /// Ask permission, then run the tool when allowed; branch on the answer.
    Permission(Box<PermissionStep>),
    // Session facts
    Plan(PlanStep),
    /// Token usage, context size and rate limits (`_meta.nori`).
    Usage(UsageStep),
    SessionInfo(SessionInfoStep),
    /// Nori's status hint: `working` or `idle`.
    Status(StatusStep),
    Mode(ModeStep),
    // Delegation through Nori's host tools
    HostTool(HostToolStep),
    Delegate(DelegateStep),
    Send(SendStep),
    // Ending a turn
    Error(ErrorSpec),
    Stop(StopStep),
    // Faults
    /// Never answer the prompt, never speak again, ignore `session/cancel`.
    Hang,
    /// Run the turn to its end, but let the prompt response vanish on the wire.
    DropResponse,
    /// Answer the prompt twice.
    RespondTwice(RespondTwice),
    /// Answer for a request id that was already answered, or was never asked.
    StaleResponse(StaleResponse),
    /// Answer the prompt now and keep working: the steps run after the turn has ended.
    Detach(Detach),
    /// Close the connection abruptly; a served process exits with `exit_code`.
    Crash(Crash),
    /// Put a line on the wire that is not what ACP says.
    WireNoise(WireNoise),
    /// Close the agent's standard output while the process lives on.
    CloseStdout,
    /// Slow what the agent writes to the client to `bytes_per_second`, as a full pipe does.
    Throttle(Throttle),
    /// Write lines to standard error of a served process, faster than anyone reads them.
    StderrFlood(StderrFlood),
    /// Send any `session/update` payload verbatim.
    RawUpdate(RawUpdate),
    // Composition
    /// Run lanes at once; their updates interleave.
    Par(Par),
    /// Run steps on another session of this agent (interleaved sessions), or on a child lane.
    Lane(Lane),
    /// A child session the agent itself reports, with its own activity, as Claude does.
    Subagent(Box<Subagent>),
    /// A message between sessions, as the agent reports it.
    PeerMessage(PeerMessage),
    Repeat(Repeat),
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Say {
    pub text: Text,
    pub chunk: Chunking,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Wait {
    pub duration: Span,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityDef {
    High,
    #[default]
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatusDef {
    #[default]
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlanEntryDef {
    pub content: String,
    pub priority: PriorityDef,
    pub status: PlanStatusDef,
}

impl Default for PlanEntryDef {
    fn default() -> Self {
        Self {
            content: String::new(),
            priority: PriorityDef::Medium,
            status: PlanStatusDef::Pending,
        }
    }
}

/// The complete plan; each plan step replaces the one before.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub entries: Vec<PlanEntryDef>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TokensDef {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

/// One rate-limit window. `resets_in` is relative to the world's clock when the step runs.
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitDef {
    pub id: String,
    pub label: String,
    pub used_percent: Option<f64>,
    pub resets_in: Option<Span>,
    /// RFC 3339; wins over `resets_in`.
    pub resets_at: Option<String>,
    pub window_minutes: Option<i64>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UsageStep {
    /// Tokens in context; with `context_size`, sends an ACP `usage_update`. Without them the
    /// report rides a `session_info_update`.
    pub context_used: Option<i64>,
    pub context_size: Option<i64>,
    /// Defaults to the session's current model.
    pub model: Option<String>,
    pub account: Option<String>,
    pub tokens: Option<TokensDef>,
    /// The tokens are the session's running total, not the change since the last report.
    pub session_total: bool,
    pub rate_limits: Vec<RateLimitDef>,
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SessionInfoStep {
    pub title: Option<String>,
    /// Extra `_meta`, merged with Nori's own keys.
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusState {
    Working,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusStep {
    pub state: StatusState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeStep {
    pub id: String,
}

/// Call any host tool as a tool card: it is announced, run through Nori, and ended with what
/// came back. The text of the result is bound to `bind` for later steps (`{{bind}}`, and
/// `{{bind.field}}` for a JSON result).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HostToolStep {
    pub name: String,
    pub arguments: Value,
    pub id: Option<String>,
    pub title: Option<String>,
    pub bind: Option<String>,
    /// The call is expected to fail; a success (or the reverse) is recorded as a divergence.
    pub expect_error: bool,
}

impl Default for HostToolStep {
    fn default() -> Self {
        Self {
            name: String::new(),
            arguments: Value::Object(serde_json::Map::new()),
            id: None,
            title: None,
            bind: None,
            expect_error: false,
        }
    }
}

/// `delegate_task`. Nori starts the child on `agent`; its scripted agent answers the task.
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DelegateStep {
    pub task: Text,
    pub agent: Option<String>,
    pub model: Option<String>,
    pub title: Option<String>,
    pub alias: Option<String>,
    pub role: Option<String>,
    /// `background` (default) or `wait`.
    pub mode: Option<String>,
    pub isolation: Option<String>,
    pub bind: Option<String>,
}

/// `task_send`: a message to any actor by alias, `actor://alias`, session id or `task-N`.
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SendStep {
    pub to: String,
    pub message: Text,
    pub steer: bool,
    pub mode: Option<String>,
    pub bind: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReasonDef {
    #[default]
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StopStep {
    pub reason: StopReasonDef,
}

impl Default for StopStep {
    fn default() -> Self {
        Self {
            reason: StopReasonDef::EndTurn,
        }
    }
}

/// What kind of failure, in the vocabulary of Nori's recovery policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// A usage window is spent (`usage_limit_reached`).
    Quota,
    /// HTTP 404 for the model.
    ModelUnavailable,
    /// The model id was never filled in (`MODEL_PLACEHOLDER`).
    ModelPlaceholder,
    /// Overloaded (`overloaded`, HTTP 529).
    Capacity,
    RateLimited,
    ServerError,
    Network,
    Auth,
    PromptTooLong,
    SessionNotFound,
    #[default]
    Internal,
    /// Exactly `code`, `message` and `data`, for shapes the other classes do not make.
    Raw,
}

/// A structured ACP error, in the fields `classify_failure` reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ErrorSpec {
    pub failure: ErrorClass,
    /// The reason a person reads (`data.detail`).
    pub detail: Option<String>,
    /// `data.status`.
    pub status: Option<i64>,
    /// `data.retry_after`, seconds.
    pub retry_after: Option<i64>,
    /// `data.resetsAt`, RFC 3339.
    pub resets_at: Option<String>,
    /// `data.resetsAt` relative to the world's clock.
    pub resets_in: Option<Span>,
    /// JSON-RPC code, for `raw`.
    pub code: Option<i64>,
    pub message: Option<String>,
    /// `data`, for `raw`.
    pub data: Option<Value>,
}

impl Default for ErrorSpec {
    fn default() -> Self {
        Self {
            failure: ErrorClass::Internal,
            detail: None,
            status: None,
            retry_after: None,
            resets_at: None,
            resets_in: None,
            code: None,
            message: None,
            data: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RespondTwice {
    /// Time between the two responses.
    pub gap: Span,
    /// The second response's stop reason; the first one's when absent.
    pub second_stop_reason: Option<StopReasonDef>,
}

impl Default for RespondTwice {
    fn default() -> Self {
        Self {
            gap: Span::ZERO,
            second_stop_reason: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleId {
    /// The id of the session's previous prompt, already answered.
    #[default]
    Previous,
    /// An id the client never used.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StaleResponse {
    pub id: StaleId,
    /// After the real response.
    pub gap: Span,
}

impl Default for StaleResponse {
    fn default() -> Self {
        Self {
            id: StaleId::Previous,
            gap: Span::ZERO,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Detach {
    /// Pause after the response before the steps begin.
    pub after: Span,
    pub steps: Vec<Step>,
}

impl Default for Detach {
    fn default() -> Self {
        Self {
            after: Span::ZERO,
            steps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Crash {
    pub exit_code: i64,
    /// Written to standard error of a served process.
    pub message: Option<String>,
    /// How a served process dies: by exiting with `exit_code`, or killed by a signal.
    pub mode: CrashMode,
}

impl Default for Crash {
    fn default() -> Self {
        Self {
            exit_code: 1,
            message: None,
            mode: CrashMode::Exit,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrashMode {
    /// The process exits with `exit_code` (0 included).
    #[default]
    Exit,
    /// The process is killed by SIGKILL: no exit code, no goodbye.
    Sigkill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireNoise {
    /// Written verbatim.
    pub line: String,
    /// End the line. Without it the line is a partial one, and whatever follows continues it.
    #[serde(default = "newline_by_default")]
    pub newline: bool,
}

fn newline_by_default() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Throttle {
    pub bytes_per_second: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StderrFlood {
    pub lines: i64,
    pub bytes_per_line: i64,
}

impl Default for StderrFlood {
    fn default() -> Self {
        Self {
            lines: 1000,
            bytes_per_line: 200,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RawUpdate {
    /// The `update` object of a `session/update`.
    pub update: Value,
    /// The lane (session) it goes on; the turn's own when absent.
    pub lane: Option<String>,
}

impl Default for RawUpdate {
    fn default() -> Self {
        Self {
            update: Value::Null,
            lane: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Par {
    pub lanes: Vec<Vec<Step>>,
}

/// Steps on another session. `session` is `self`, `previous` (the session opened before this
/// one), `first`, `newest`, or a session id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lane {
    pub session: String,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentEnd {
    /// Left running: the parent's turn ends first, as with a background agent.
    Running,
    #[default]
    Idle,
    Completed,
    Failed,
    Cancelled,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Subagent {
    /// The child's session id; generated when absent. Reusing an id addresses the same child.
    pub id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub model: Option<String>,
    /// What the parent told the child, shown as a message.
    pub prompt: Option<Text>,
    /// The child's own activity, on its own session id.
    pub steps: Vec<Step>,
    pub ends: SubagentEnd,
    pub reason: Option<String>,
}

impl Default for Subagent {
    fn default() -> Self {
        Self {
            id: None,
            title: String::new(),
            description: None,
            model: None,
            prompt: None,
            steps: Vec::new(),
            ends: SubagentEnd::Idle,
            reason: None,
        }
    }
}

/// A `session_message`: `from` told `to` something. Both are lane ids (`self` is the turn's
/// own session); the message is delivered on `to`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PeerMessage {
    pub from: String,
    pub to: String,
    pub text: Text,
    pub id: Option<String>,
}

impl Default for PeerMessage {
    fn default() -> Self {
        Self {
            from: "self".to_owned(),
            to: "self".to_owned(),
            text: Text::default(),
            id: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repeat {
    pub times: i64,
    /// Pause between repetitions. `{{i}}` is the 1-based repetition inside the steps.
    #[serde(default)]
    pub every: Span,
    pub steps: Vec<Step>,
}
