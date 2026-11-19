//! The client-facing view of a session: events the presentation layer produces, and the
//! snapshots (tools, approvals, plans, messages) they carry.

use std::collections::HashMap;
use std::path::PathBuf;

use nori_protocol::CommandAvailability;
use nori_protocol::ThreadGoal;
use nori_protocol::acp::MaybeUndefined;
use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::DelegationCall;
use super::NativeCall;
use super::session_runtime;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
pub enum ClientEvent {
    ToolSnapshot(ToolSnapshot),
    ApprovalRequest(ApprovalRequest),
    MessageDelta(MessageDelta),
    PlanSnapshot(PlanSnapshot),
    SessionPhaseChanged(session_runtime::SessionPhaseView),
    PromptCompleted(PromptCompleted),
    LoadCompleted,
    QueueChanged(QueueChanged),
    ContextCompacted(ContextCompacted),
    ReplayEntry(ReplayEntry),
    AgentCommandsUpdate(AgentCommandsUpdate),
    SessionCapabilitiesChanged(SessionCapabilitiesView),
    SessionUpdateInfo(SessionUpdateInfo),
    SessionConfigUpdate(SessionConfigUpdate),
    SessionModeChanged(SessionModeChanged),
    ThreadGoalUpdated(ThreadGoalUpdated),
    ThreadGoalCleared,
    Warning(WarningInfo),
}

/// A warning emitted by the session runtime (e.g. out-of-phase content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WarningInfo {
    pub message: String,
}

/// How a prompt turn failed, when it did. Carried on `PromptCompleted` so a
/// single event determines both how the failure is surfaced and whether an
/// unattended loop should keep going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnFailure {
    /// A transient failure (e.g. a momentary API overload or rate limit) that
    /// a retry can recover from.
    Retryable,
    /// A persistent failure that a retry cannot fix.
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PromptCompleted {
    pub stop_reason: acp::StopReason,
    pub last_agent_message: Option<String>,
    /// `Some` when the turn ended in a failure; `None` for a clean completion
    /// (success or a user-requested cancellation).
    #[serde(default)]
    pub failure: Option<TurnFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct QueueChanged {
    pub prompts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ContextCompacted {
    pub summary: Option<String>,
}

/// A set of commands advertised by the ACP agent.
/// Each update fully replaces the previous set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentCommandsUpdate {
    pub commands: Vec<AgentCommandInfo>,
}

/// Information about a single agent-provided slash command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentCommandInfo {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

/// Snapshot of the active session capabilities as projected for Nori clients.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionCapabilitiesView {
    pub agent: AgentCapabilitiesView,
    #[serde(default)]
    pub nori_client: NoriClientCapabilitiesView,
    pub builtin_commands: HashMap<String, CommandAvailability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentCapabilitiesView {
    pub http_mcp: bool,
    pub load_session: bool,
    /// Whether the agent advertises the ACP `session/list` capability.
    #[serde(default)]
    pub session_list: bool,
    /// Whether the agent advertises the ACP `session/resume` capability.
    #[serde(default)]
    pub session_resume: bool,
    /// Whether the agent advertises the ACP `session/close` capability.
    #[serde(default)]
    pub session_close: bool,
    /// Whether the agent advertises the ACP `session/fork` capability
    /// (branch-at-head — the `/fork` "Branch from current point" path).
    #[serde(default)]
    pub session_fork: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NoriClientCapabilitiesView {
    pub advertised: bool,
    pub initialized: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionConfigUpdate {
    pub config_options: Vec<acp::SessionConfigOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionUpdateInfo {
    pub kind: SessionUpdateKind,
    pub message: String,
    pub hint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<session_runtime::SessionUsageState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_info_patch: Option<SessionInfoPatch>,
}

/// Structured ACP session metadata retained alongside the legacy text
/// projection so UI consumers can render it without parsing a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionInfoPatch {
    pub title: MaybeUndefined<String>,
    pub updated_at: MaybeUndefined<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<acp::Meta>,
}

/// Emitted when the agent reports its active mode has changed.
///
/// The TUI resolves `current_mode_id` to a human label using its cached mode
/// list; the protocol does not carry the label so it stays UI-agnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct SessionModeChanged {
    pub current_mode_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ThreadGoalUpdated {
    pub goal: ThreadGoal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionUpdateKind {
    ConfigOptions,
    SessionInfo,
    Usage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "replay_type", rename_all = "snake_case")]
pub enum ReplayEntry {
    UserMessage { text: String },
    AssistantMessage { text: String },
    ReasoningMessage { text: String },
    PlanSnapshot { snapshot: PlanSnapshot },
    ToolSnapshot { snapshot: Box<ToolSnapshot> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct MessageDelta {
    pub stream: MessageStream,
    pub message_id: Option<String>,
    pub delta: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStream {
    User,
    Answer,
    Reasoning,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanSnapshot {
    pub entries: Vec<PlanEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PlanEntry {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolSnapshot {
    pub call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub phase: ToolPhase,
    pub locations: Vec<ToolLocation>,
    pub invocation: Option<Invocation>,
    pub artifacts: Vec<Artifact>,
    pub raw_input: Option<serde_json::Value>,
    pub raw_output: Option<serde_json::Value>,
    /// The request that created this tool call. Used for cancellation
    /// and request-local rendering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_request_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApprovalRequest {
    pub call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub options: Vec<ApprovalOption>,
    pub subject: ApprovalSubject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "subject_type", rename_all = "snake_case")]
pub enum ApprovalSubject {
    ToolSnapshot(ToolSnapshot),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ApprovalOption {
    pub option_id: String,
    pub name: String,
    pub kind: ApprovalOptionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOptionKind {
    AllowAlways,
    AllowOnce,
    RejectOnce,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Search,
    Execute,
    Create,
    Edit,
    Delete,
    Move,
    Fetch,
    Think,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPhase {
    Pending,
    PendingApproval,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolLocation {
    pub path: PathBuf,
    pub line: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "invocation_type", rename_all = "snake_case")]
pub enum Invocation {
    FileChanges {
        changes: Vec<FileChange>,
    },
    FileOperations {
        operations: Vec<FileOperation>,
    },
    Command {
        command: String,
    },
    Read {
        path: PathBuf,
    },
    Search {
        query: Option<String>,
        path: Option<PathBuf>,
    },
    ListFiles {
        path: Option<PathBuf>,
    },
    Tool {
        tool_name: String,
        input: Option<serde_json::Value>,
    },
    RawJson(serde_json::Value),
    /// A call to one of Nori's delegated-task tools.
    Delegation(DelegationCall),
    /// A call to one of Nori's git or goal tools.
    Native(NativeCall),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "artifact_type", rename_all = "snake_case")]
pub enum Artifact {
    Diff(FileChange),
    Text {
        text: String,
        /// How the text is meant to be read, as its source declared it.
        #[serde(default)]
        format: TextFormat,
    },
}

/// How a piece of text is meant to be read. Text is plain unless its source says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextFormat {
    #[default]
    Plain,
    Markdown,
}

impl TextFormat {
    /// The format an embedded resource declares: markdown when its MIME type is
    /// `text/markdown` (or `text/x-markdown`, parameters aside), or when it has no MIME type and
    /// its URI names a `.md`, `.markdown`, or `.mdx` file.
    pub fn of_resource(mime_type: Option<&str>, uri: &str) -> Self {
        if let Some(mime_type) = mime_type {
            let essence = mime_type
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            return match essence.as_str() {
                "text/markdown" | "text/x-markdown" => Self::Markdown,
                _ => Self::Plain,
            };
        }
        let path = uri.split(['?', '#']).next().unwrap_or_default();
        let extension = path
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_lowercase());
        match extension.as_deref() {
            Some("md" | "markdown" | "mdx") => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FileChange {
    pub path: PathBuf,
    pub old_text: Option<String>,
    pub new_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation_type", rename_all = "snake_case")]
pub enum FileOperation {
    Create {
        path: PathBuf,
        new_text: String,
    },
    Update {
        path: PathBuf,
        old_text: String,
        new_text: String,
    },
    Delete {
        path: PathBuf,
        old_text: Option<String>,
    },
    Move {
        from_path: PathBuf,
        to_path: PathBuf,
        old_text: Option<String>,
        new_text: Option<String>,
    },
}
