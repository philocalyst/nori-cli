//! The session's channel back to the client: notifications, permission questions and the
//! user-input round trip. Behind a trait so tests record what would have been sent.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use crate::thread::exec::ActiveCommand;
use codex_protocol::plan_tool::PlanItemArg;
use codex_protocol::plan_tool::StepStatus;
use nori_acp_kit::acp::Client;
use nori_acp_kit::acp::ConnectionTo;
use nori_acp_kit::acp::Error;
use nori_acp_kit::acp::UntypedMessage;
use nori_acp_kit::schema::ClientCapabilities;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::MessageId;
use nori_acp_kit::schema::PermissionOption;
use nori_acp_kit::schema::Plan;
use nori_acp_kit::schema::PlanEntry;
use nori_acp_kit::schema::PlanEntryPriority;
use nori_acp_kit::schema::PlanEntryStatus;
use nori_acp_kit::schema::RequestPermissionRequest;
use nori_acp_kit::schema::RequestPermissionResponse;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionNotification;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;
use serde_json::json;
use tracing::error;
use uuid::Uuid;

/// Abstraction over the ACP connection for sending notifications and requests
/// back to the client. This replaces the old `Client` trait usage.
pub(super) trait ClientSender: Send + Sync + 'static {
    fn send_session_notification(&self, notif: SessionNotification) -> Result<(), Error>;
    fn send_untyped_notification(&self, params: serde_json::Value) -> Result<(), Error>;
    fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>>;
    fn request_user_input(
        &self,
        _params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, Error>> + Send + '_>> {
        Box::pin(async { Err(Error::method_not_found()) })
    }
    fn send_question_cancelled(&self, _params: serde_json::Value) -> Result<(), Error> {
        Ok(())
    }
}

/// Production implementation that wraps a `ConnectionTo<Client>`.
pub(super) struct AcpConnection(ConnectionTo<Client>);

impl ClientSender for AcpConnection {
    fn send_session_notification(&self, notif: SessionNotification) -> Result<(), Error> {
        self.0.send_notification(notif)
    }

    fn send_untyped_notification(&self, params: serde_json::Value) -> Result<(), Error> {
        self.0
            .send_notification(UntypedMessage::new("session/update", params)?)
    }

    fn request_permission(
        &self,
        req: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>> {
        Box::pin(async move { self.0.send_request(req).block_task().await })
    }

    fn request_user_input(
        &self,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, Error>> + Send + '_>> {
        Box::pin(async move {
            self.0
                .send_request(UntypedMessage::new("_nori/request_user_input", params)?)
                .block_task()
                .await
        })
    }

    fn send_question_cancelled(&self, params: serde_json::Value) -> Result<(), Error> {
        self.0
            .send_notification(UntypedMessage::new("_nori/user_input_cancelled", params)?)
    }
}

#[derive(Clone)]
pub(super) struct SessionClient {
    pub(super) session_id: SessionId,
    pub(super) client: Arc<dyn ClientSender>,
    pub(super) client_capabilities: Arc<Mutex<ClientCapabilities>>,
}

impl SessionClient {
    pub(super) fn new(
        session_id: SessionId,
        cx: ConnectionTo<Client>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
    ) -> Self {
        Self {
            session_id,
            client: Arc::new(AcpConnection(cx)),
            client_capabilities,
        }
    }

    #[cfg(test)]
    pub(super) fn with_client(
        session_id: SessionId,
        client: Arc<dyn ClientSender>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
    ) -> Self {
        Self {
            session_id,
            client,
            client_capabilities,
        }
    }

    pub(super) fn supports_terminal_output(&self, active_command: &ActiveCommand) -> bool {
        active_command.terminal_output
            && self
                .client_capabilities
                .lock()
                .unwrap()
                .meta
                .as_ref()
                .is_some_and(|v| {
                    v.get("terminal_output")
                        .is_some_and(|v| v.as_bool().unwrap_or_default())
                })
    }

    pub(super) fn send_notification(&self, update: SessionUpdate) {
        if let Err(e) = self
            .client
            .send_session_notification(SessionNotification::new(self.session_id.clone(), update))
        {
            error!("Failed to send session notification: {:?}", e);
        }
    }

    pub(super) fn announce_subagent(
        &self,
        call_id: &str,
        child_session: &str,
        title: &str,
        task: &str,
    ) {
        let params = json!({
            "sessionId": self.session_id.to_string(),
            "update": {
                "sessionUpdate": "subagent_spawned",
                "subagentSessionId": child_session,
                "name": title,
                "task": task,
                "state": "running"
            }
        });
        if let Err(error) = self.client.send_untyped_notification(params) {
            error!(?error, call_id, "failed to announce Codex subagent");
            return;
        }
        let link = json!({
            "sessionId": self.session_id.to_string(),
            "update": {
                "sessionUpdate": "session_message",
                "messageId": call_id,
                "senderSessionId": self.session_id.to_string(),
                "recipientSessionId": child_session,
                "content": [{"type": "text", "text": task}]
            }
        });
        if let Err(error) = self.client.send_untyped_notification(link) {
            error!(?error, call_id, "failed to link Codex subagent");
        }
    }

    pub(super) fn send_user_message(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::UserMessageChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    pub(super) fn send_agent_text(&self, text: impl Into<String>) {
        self.send_agent_text_with_id(text, Uuid::new_v4().to_string());
    }

    pub(super) fn send_agent_text_with_id(
        &self,
        text: impl Into<String>,
        message_id: impl Into<String>,
    ) {
        self.send_notification(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(text.into().into()).message_id(MessageId::new(message_id.into())),
        ));
    }

    pub(super) fn send_agent_thought(&self, text: impl Into<String>) {
        self.send_notification(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
            text.into().into(),
        )));
    }

    pub(super) fn send_tool_call(&self, tool_call: ToolCall) {
        self.send_notification(SessionUpdate::ToolCall(tool_call));
    }

    pub(super) fn send_tool_call_update(&self, update: ToolCallUpdate) {
        self.send_notification(SessionUpdate::ToolCallUpdate(update));
    }

    /// Send a completed tool call (used for replay and simple cases)
    pub(super) fn send_completed_tool_call(
        &self,
        call_id: impl Into<ToolCallId>,
        title: impl Into<String>,
        kind: ToolKind,
        raw_input: Option<serde_json::Value>,
    ) {
        let mut tool_call = ToolCall::new(call_id, title)
            .kind(kind)
            .status(ToolCallStatus::Completed);
        if let Some(input) = raw_input {
            tool_call = tool_call.raw_input(input);
        }
        self.send_tool_call(tool_call);
    }

    /// Send a tool call completion update (used for replay)
    pub(super) fn send_tool_call_completed(
        &self,
        call_id: impl Into<ToolCallId>,
        raw_output: Option<serde_json::Value>,
    ) {
        let mut fields = ToolCallUpdateFields::new().status(ToolCallStatus::Completed);
        if let Some(output) = raw_output {
            fields = fields.raw_output(output);
        }
        self.send_tool_call_update(ToolCallUpdate::new(call_id, fields));
    }

    pub(super) fn update_plan(&self, plan: Vec<PlanItemArg>) {
        self.send_notification(SessionUpdate::Plan(Plan::new(
            plan.into_iter()
                .map(|entry| {
                    PlanEntry::new(
                        entry.step,
                        PlanEntryPriority::Medium,
                        match entry.status {
                            StepStatus::Pending => PlanEntryStatus::Pending,
                            StepStatus::InProgress => PlanEntryStatus::InProgress,
                            StepStatus::Completed => PlanEntryStatus::Completed,
                        },
                    )
                })
                .collect(),
        )));
    }

    pub(super) async fn request_permission(
        &self,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
    ) -> Result<RequestPermissionResponse, Error> {
        self.client
            .request_permission(RequestPermissionRequest::new(
                self.session_id.clone(),
                tool_call,
                options,
            ))
            .await
    }
}
