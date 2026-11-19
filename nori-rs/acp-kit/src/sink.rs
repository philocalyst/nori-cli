//! Sending a session's updates and questions to the client.

use agent_client_protocol as acp;

use crate::schema::AvailableCommand;
use crate::schema::AvailableCommandsUpdate;
use crate::schema::ConfigOptionUpdate;
use crate::schema::ContentBlock;
use crate::schema::ContentChunk;
use crate::schema::PermissionOption;
use crate::schema::PermissionOptionId;
use crate::schema::RequestPermissionOutcome;
use crate::schema::RequestPermissionRequest;
use crate::schema::SessionConfigOption;
use crate::schema::SessionId;
use crate::schema::SessionInfoUpdate;
use crate::schema::SessionNotification;
use crate::schema::SessionUpdate;
use crate::schema::ToolCall;
use crate::schema::ToolCallUpdate;
use crate::schema::UsageUpdate;
use crate::usage::UsageReport;

/// The connection to the client, as every handler receives it. Cheap to clone.
pub type Cx = acp::ConnectionTo<acp::Client>;

/// The answer to a permission question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionDecision {
    Selected(PermissionOptionId),
    /// The client withdrew the question, because the turn was cancelled or the connection ended.
    Cancelled,
}

/// How full the model's context window is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub used: i64,
    pub size: i64,
}

/// The update that reports usage. With a known context size it is a `UsageUpdate`; without one the
/// report rides a `session_info_update`, which carries `_meta` alone. `None` when the provider
/// reported nothing at all. For an agent that sends its updates by another route than [`Sink`].
pub fn usage_update(context: Option<ContextUsage>, report: &UsageReport) -> Option<SessionUpdate> {
    match context {
        Some(context) => {
            let used = u64::try_from(context.used.max(0)).unwrap_or_default();
            let size = u64::try_from(context.size.max(0)).unwrap_or_default();
            let mut update = UsageUpdate::new(used, size);
            if !report.is_empty() {
                update = update.meta(report.meta());
            }
            Some(SessionUpdate::UsageUpdate(update))
        }
        None if report.is_empty() => None,
        None => Some(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().meta(report.meta()),
        )),
    }
}

/// One session's channel to the client. Owned and cloneable, so a spawned task can keep
/// talking to the client after the handler that made it has returned.
#[derive(Clone)]
pub struct Sink {
    cx: Cx,
    session: SessionId,
}

impl std::fmt::Debug for Sink {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Sink")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Sink {
    pub fn new(cx: &Cx, session: SessionId) -> Self {
        Self {
            cx: cx.clone(),
            session,
        }
    }

    pub fn cx(&self) -> &Cx {
        &self.cx
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session
    }

    pub fn update(&self, update: SessionUpdate) -> acp::Result<()> {
        self.cx
            .send_notification(SessionNotification::new(self.session.clone(), update))
    }

    /// A chunk of the assistant's answer.
    pub fn text(&self, text: impl Into<String>) -> acp::Result<()> {
        self.update(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::from(text.into()),
        )))
    }

    /// A chunk of the assistant's reasoning.
    pub fn thought(&self, text: impl Into<String>) -> acp::Result<()> {
        self.update(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            ContentBlock::from(text.into()),
        )))
    }

    pub fn tool_call(&self, call: ToolCall) -> acp::Result<()> {
        self.update(SessionUpdate::ToolCall(call))
    }

    pub fn tool_update(&self, update: ToolCallUpdate) -> acp::Result<()> {
        self.update(SessionUpdate::ToolCallUpdate(update))
    }

    /// Report usage: see [`usage_update`]. Nothing is sent when there is nothing to report.
    pub fn usage(&self, context: Option<ContextUsage>, report: &UsageReport) -> acp::Result<()> {
        match usage_update(context, report) {
            Some(update) => self.update(update),
            None => Ok(()),
        }
    }

    /// The session's configuration options changed on the agent's own initiative.
    pub fn config_options(&self, options: Vec<SessionConfigOption>) -> acp::Result<()> {
        self.update(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
            options,
        )))
    }

    /// The session's slash commands, as the agent knows them.
    pub fn commands(&self, commands: Vec<AvailableCommand>) -> acp::Result<()> {
        self.update(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(commands),
        ))
    }

    /// Ask the client whether `tool` may run, and wait for the answer.
    pub async fn ask_permission(
        &self,
        tool: ToolCallUpdate,
        options: Vec<PermissionOption>,
    ) -> acp::Result<PermissionDecision> {
        let response = self
            .cx
            .send_request(RequestPermissionRequest::new(
                self.session.clone(),
                tool,
                options,
            ))
            .block_task()
            .await?;
        Ok(match response.outcome {
            RequestPermissionOutcome::Selected(selected) => {
                PermissionDecision::Selected(selected.option_id)
            }
            RequestPermissionOutcome::Cancelled => PermissionDecision::Cancelled,
            // A future outcome is not an approval.
            _ => PermissionDecision::Cancelled,
        })
    }
}
