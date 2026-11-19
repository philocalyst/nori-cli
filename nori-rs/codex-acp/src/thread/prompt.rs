//! The state of one submission and the dispatch of every Codex event it produces onto ACP
//! updates. The event families are handled in the sibling modules (`exec`, `patches`,
//! `tool_calls`, `media`, `guardian`, `permissions`).

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;

use crate::thread::ThreadMessage;
use crate::thread::backends::CodexThreadImpl;
use crate::thread::client::SessionClient;
use crate::thread::errors::turn_error;
use crate::thread::exec::ActiveCommand;
use crate::thread::permissions::PendingPermissionInteraction;
use crate::thread::usage::token_count_report;
use codex_protocol::dynamic_tools::DynamicToolCallRequest;
use codex_protocol::plan_tool::UpdatePlanArgs;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::AgentReasoningEvent;
use codex_protocol::protocol::AgentReasoningSectionBreakEvent;
use codex_protocol::protocol::CollabAgentSpawnEndEvent;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExitedReviewModeEvent;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::McpStartupCompleteEvent;
use codex_protocol::protocol::McpStartupUpdateEvent;
use codex_protocol::protocol::McpToolCallBeginEvent;
use codex_protocol::protocol::McpToolCallEndEvent;
use codex_protocol::protocol::ModelRerouteEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReasoningContentDeltaEvent;
use codex_protocol::protocol::ReasoningRawContentDeltaEvent;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::protocol::StreamErrorEvent;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use codex_protocol::protocol::ViewImageToolCallEvent;
use codex_protocol::protocol::WarningEvent;
use codex_protocol::protocol::WebSearchBeginEvent;
use codex_protocol::protocol::WebSearchEndEvent;
use codex_protocol::review_format::format_review_findings_block;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::Content;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::RequestPermissionResponse;
use nori_acp_kit::schema::ResourceLink;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallLocation;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolKind;
use nori_acp_kit::usage_update;
use nori_host_tools::HostTools;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::error;
use tracing::info;
use tracing::warn;

pub(super) enum SubmissionState {
    /// User prompts, including slash commands like /init, /review, /compact.
    Prompt(PromptState),
}

impl SubmissionState {
    pub(super) fn is_active(&self) -> bool {
        match self {
            Self::Prompt(state) => state.is_active(),
        }
    }

    pub(super) async fn handle_event(&mut self, client: &SessionClient, event: EventMsg) {
        match self {
            Self::Prompt(state) => state.handle_event(client, event).await,
        }
    }

    pub(super) async fn handle_permission_request_resolved(
        &mut self,
        client: &SessionClient,
        interaction_id: u64,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    ) -> Result<(), Error> {
        match self {
            Self::Prompt(state) => {
                state
                    .handle_permission_request_resolved(
                        client,
                        interaction_id,
                        request_key,
                        response,
                    )
                    .await
            }
        }
    }

    pub(super) fn detach_pending_interactions(&mut self) {
        match self {
            Self::Prompt(state) => {
                state.detach_pending_interactions();
            }
        }
    }

    pub(super) fn fail(&mut self, err: Error) {
        if let Self::Prompt(state) = self
            && let Some(response_tx) = state.response_tx.take()
        {
            drop(response_tx.send(Err(err)));
        }
    }
}

pub(super) struct PromptState {
    pub(super) submission_id: String,
    pub(super) active_commands: HashMap<String, ActiveCommand>,
    pub(super) active_web_search: Option<String>,
    pub(super) active_image_generations: HashSet<String>,
    pub(super) active_guardian_assessments: HashSet<String>,
    pub(super) thread: Arc<dyn CodexThreadImpl>,
    pub(super) resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
    pub(super) pending_permission_interactions: HashMap<String, PendingPermissionInteraction>,
    pub(super) next_permission_interaction_id: u64,
    pub(super) event_count: usize,
    pub(super) response_tx: Option<oneshot::Sender<Result<StopReason, Error>>>,
    pub(super) seen_message_deltas: bool,
    pub(super) seen_reasoning_deltas: bool,
    /// The model the turn runs on, for the usage report; `None` where it is not known.
    pub(super) model: Option<String>,
    /// The host's tools, which answer this turn's dynamic tool calls.
    pub(super) host_tools: Option<Arc<dyn HostTools>>,
    pub(super) host_tool_cancellation: tokio_util::sync::CancellationToken,
    pub(super) host_goal_contexts: crate::host_tools::GoalContexts,
    /// The gate's turn, for a prompt that holds one: the session runs while it lives, and its
    /// cancellation decides the stop reason.
    pub(super) turn: Option<nori_acp_kit::Turn>,
}
impl PromptState {
    pub(super) fn new(
        submission_id: String,
        thread: Arc<dyn CodexThreadImpl>,
        resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
        response_tx: oneshot::Sender<Result<StopReason, Error>>,
    ) -> Self {
        Self {
            submission_id,
            active_commands: HashMap::new(),
            active_web_search: None,
            active_image_generations: HashSet::new(),
            active_guardian_assessments: HashSet::new(),
            thread,
            resolution_tx,
            pending_permission_interactions: HashMap::new(),
            next_permission_interaction_id: 0,
            event_count: 0,
            response_tx: Some(response_tx),
            seen_message_deltas: false,
            seen_reasoning_deltas: false,
            model: None,
            host_tools: None,
            host_tool_cancellation: tokio_util::sync::CancellationToken::new(),
            host_goal_contexts: Arc::default(),
            turn: None,
        }
    }

    pub(super) fn is_active(&self) -> bool {
        let Some(response_tx) = &self.response_tx else {
            return false;
        };
        !response_tx.is_closed()
    }

    pub(super) fn detach_pending_interactions(&mut self) {
        // Keep detached permission request tasks running so ACP can route the
        // client's required `Cancelled` response after session cancellation.
        self.pending_permission_interactions.clear();
        self.host_tool_cancellation.cancel();
    }

    #[expect(clippy::too_many_lines)]
    pub(super) async fn handle_event(&mut self, client: &SessionClient, event: EventMsg) {
        self.event_count += 1;

        // Complete any previous web search before starting a new one
        match &event {
            EventMsg::Error(..)
            | EventMsg::StreamError(..)
            | EventMsg::WebSearchBegin(..)
            | EventMsg::UserMessage(..)
            | EventMsg::ExecApprovalRequest(..)
            | EventMsg::ImageGenerationBegin(..)
            | EventMsg::ImageGenerationEnd(..)
            | EventMsg::ExecCommandBegin(..)
            | EventMsg::ExecCommandOutputDelta(..)
            | EventMsg::ExecCommandEnd(..)
            | EventMsg::McpToolCallBegin(..)
            | EventMsg::McpToolCallEnd(..)
            | EventMsg::ApplyPatchApprovalRequest(..)
            | EventMsg::PatchApplyBegin(..)
            | EventMsg::PatchApplyEnd(..)
            | EventMsg::TurnStarted(..)
            | EventMsg::TurnComplete(..)
            | EventMsg::TurnDiff(..)
            | EventMsg::TurnAborted(..)
            | EventMsg::EnteredReviewMode(..)
            | EventMsg::ExitedReviewMode(..)
            | EventMsg::ShutdownComplete => {
                self.complete_web_search(client);
            }
            _ => {}
        }

        match event {
            EventMsg::TurnStarted(TurnStartedEvent {
                model_context_window,
                collaboration_mode_kind,
                turn_id,
                ..
            }) => {
                info!("Task started with context window of {turn_id} {model_context_window:?} {collaboration_mode_kind:?}");
            }
            EventMsg::TokenCount(event) => {
                if let Some((context, report)) = token_count_report(self.model.as_deref(), &event)
                    && let Some(update) = usage_update(context, &report)
                {
                    client.send_notification(update);
                }
            }
            EventMsg::ItemStarted(ItemStartedEvent { thread_id, turn_id, item , started_at_ms: _}) => {
                info!("Item started with thread_id: {thread_id}, turn_id: {turn_id}, item: {item:?}");
            }
            EventMsg::UserMessage(UserMessageEvent {
                message,
                ..
            }) => {
                info!("User message: {message:?}");
            }
            EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
            }) => {
                info!("Agent message content delta received: thread_id: {thread_id}, turn_id: {turn_id}, item_id: {item_id}, delta: {delta:?}");
                self.seen_message_deltas = true;
                client.send_agent_text_with_id(delta, item_id);
            }
            EventMsg::ReasoningContentDelta(ReasoningContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                summary_index: index,
            })
            | EventMsg::ReasoningRawContentDelta(ReasoningRawContentDeltaEvent {
                thread_id,
                turn_id,
                item_id,
                delta,
                content_index: index,
            }) => {
                info!("Agent reasoning content delta received: thread_id: {thread_id}, turn_id: {turn_id}, item_id: {item_id}, index: {index}, delta: {delta:?}");
                self.seen_reasoning_deltas = true;
                client.send_agent_thought(delta);
            }
            EventMsg::AgentReasoningSectionBreak(AgentReasoningSectionBreakEvent {
                item_id,
                summary_index,
            }) => {
                info!("Agent reasoning section break received:  item_id: {item_id}, index: {summary_index}");
                // Make sure the section heading actually get spacing
                self.seen_reasoning_deltas = true;
                client.send_agent_thought("\n\n");
            }
            EventMsg::AgentMessage(AgentMessageEvent { message , .. }) => {
                info!("Agent message (non-delta) received: {message:?}");
                // We didn't receive this message via streaming
                if !std::mem::take(&mut self.seen_message_deltas) {
                    client.send_agent_text(message);
                }
            }
            EventMsg::AgentReasoning(AgentReasoningEvent { text }) => {
                info!("Agent reasoning (non-delta) received: {text:?}");
                // We didn't receive this message via streaming
                if !std::mem::take(&mut self.seen_reasoning_deltas) {
                    client.send_agent_thought(text);
                }
            }
            EventMsg::ThreadGoalUpdated(event) => {
                info!("Thread goal updated: {:?}", event.goal.objective);
                client.send_agent_text(format_thread_goal_update(&event));
            }
            EventMsg::PlanUpdate(UpdatePlanArgs { explanation, plan }) => {
                // Send this to the client via session/update notification
                info!("Agent plan updated. Explanation: {:?}", explanation);
                client.update_plan(plan);
            }
            EventMsg::WebSearchBegin(WebSearchBeginEvent { call_id }) => {
                info!("Web search started: call_id={}", call_id);
                // Create a ToolCall notification for the search beginning
                self.start_web_search(client, call_id);
            }
            EventMsg::WebSearchEnd(WebSearchEndEvent {
                call_id,
                query,
                action, .. }) => {
                info!("Web search query received: call_id={call_id}, query={query}");
                // Send update that the search is in progress with the query
                // (WebSearchEnd just means we have the query, not that results are ready)
                self.update_web_search_query(client, call_id, query, action);
                // The actual search results will come through AgentMessage events
                // We mark as completed when a new tool call begins
            }
            EventMsg::ImageGenerationBegin(event) => {
                info!("Image generation started: call_id={}", event.call_id);
                self.start_image_generation(client, event);
            }
            EventMsg::ImageGenerationEnd(event) => {
                info!(
                    "Image generation ended: call_id={}, status={}",
                    event.call_id, event.status
                );
                self.end_image_generation(client, event);
            }
            EventMsg::ExecApprovalRequest(event) => {
                info!(
                    "Command execution started: call_id={}, command={:?}",
                    event.call_id, event.command
                );
                if let Err(err) = self.exec_approval(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::ExecCommandBegin(event) => {
                info!(
                    "Command execution started: call_id={}, command={:?}",
                    event.call_id, event.command
                );
                self.exec_command_begin(client, event);
            }
            EventMsg::ExecCommandOutputDelta(delta_event) => {
                self.exec_command_output_delta(client, delta_event);
            }
            EventMsg::ExecCommandEnd(end_event) => {
                info!(
                    "Command execution ended: call_id={}, exit_code={}",
                    end_event.call_id, end_event.exit_code
                );
                self.exec_command_end(client, end_event);
            }
            EventMsg::TerminalInteraction(event) => {
                info!(
                    "Terminal interaction: call_id={}, process_id={}, stdin={}",
                    event.call_id, event.process_id, event.stdin
                );
                self.terminal_interaction(client, event);
            }
            EventMsg::DynamicToolCallRequest(DynamicToolCallRequest { call_id, turn_id, namespace, tool, arguments, started_at_ms: _ }) => {
                info!("Dynamic tool call request: call_id={call_id}, turn_id={turn_id}, namespace={namespace:?}, tool={tool}");
                self.start_dynamic_tool_call(client, call_id, tool, arguments);
            }
            EventMsg::DynamicToolCallResponse(event) => {
                info!(
                    "Dynamic tool call response: call_id={}, turn_id={}, tool={}",
                    event.call_id, event.turn_id, event.tool
                );
                self.end_dynamic_tool_call(client, event);
            }
            EventMsg::McpToolCallBegin(McpToolCallBeginEvent {
                call_id,
                invocation,
                ..
            }) => {
                info!(
                    "MCP tool call begin: call_id={call_id}, invocation={} {}",
                    invocation.server, invocation.tool
                );
                self.start_mcp_tool_call(client, call_id, invocation);
            }
            EventMsg::McpToolCallEnd(McpToolCallEndEvent {
                call_id,
                invocation,
                duration,
                result,
                ..
            }) => {
                info!(
                    "MCP tool call ended: call_id={call_id}, invocation={} {}, duration={duration:?}",
                    invocation.server, invocation.tool
                );
                self.end_mcp_tool_call(client, call_id, result);
            }
            EventMsg::ApplyPatchApprovalRequest(event) => {
                info!(
                    "Apply patch approval request: call_id={}, reason={:?}",
                    event.call_id, event.reason
                );
                if let Err(err) = self.patch_approval(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::PatchApplyBegin(event) => {
                info!(
                    "Patch apply begin: call_id={}, auto_approved={}",
                    event.call_id, event.auto_approved
                );
                self.start_patch_apply(client, event);
            }
            EventMsg::PatchApplyUpdated(event) => {
                info!(
                    "Patch apply updated: call_id={}, change_count={}",
                    event.call_id,
                    event.changes.len()
                );
                self.update_patch_apply(client, event);
            }
            EventMsg::PatchApplyEnd(event) => {
                info!(
                    "Patch apply end: call_id={}, success={}",
                    event.call_id, event.success
                );
                self.end_patch_apply(client, event);
            }
            EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id,
                turn_id,
                item,
                .. }) => {
                info!("Item completed: thread_id={}, turn_id={}, item={:?}", thread_id, turn_id, item);
            }
            EventMsg::TurnComplete(TurnCompleteEvent { last_agent_message, turn_id, .. }) => {
                info!(
                    "Task {turn_id} completed successfully after {} events. Last agent message: {last_agent_message:?}",
                    self.event_count
                );
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    let cancelled = self.turn.as_ref().is_some_and(nori_acp_kit::Turn::is_cancelled);
                    response_tx
                        .send(Ok(if cancelled {
                            StopReason::Cancelled
                        } else {
                            StopReason::EndTurn
                        }))
                        .ok();
                }
            }
            EventMsg::StreamError(StreamErrorEvent {
                message,
                codex_error_info,
                additional_details,
            }) => {
                error!(
                    "Handled error during turn: {message} {codex_error_info:?} {additional_details:?}"
                );
            }
            EventMsg::Error(ErrorEvent {
                message,
                codex_error_info, .. }) => {
                error!("Unhandled error during turn: {message} {codex_error_info:?}");
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx
                        .send(Err(turn_error(message, codex_error_info.as_ref())))
                        .ok();
                }
            }
            EventMsg::TurnAborted(TurnAbortedEvent { reason, turn_id, .. }) => {
                info!("Turn {turn_id:?} aborted: {reason:?}");
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::Cancelled)).ok();
                }
            }
            EventMsg::ShutdownComplete => {
                info!("Agent shutting down");
                self.detach_pending_interactions();
                if let Some(response_tx) = self.response_tx.take() {
                    response_tx.send(Ok(StopReason::Cancelled)).ok();
                }
            }
            EventMsg::ViewImageToolCall(ViewImageToolCallEvent { call_id, path }) => {
                info!("ViewImageToolCallEvent received");
                let display_path = path.to_path_buf().display().to_string();
                client.send_notification(
                    SessionUpdate::ToolCall(
                        ToolCall::new(call_id, format!("View Image {display_path}"))
                            .kind(ToolKind::Read).status(ToolCallStatus::Completed)
                            .content(vec![ToolCallContent::Content(Content::new(ContentBlock::ResourceLink(ResourceLink::new(display_path.clone(), display_path.clone())
                        )
                    )
                )]).locations(vec![ToolCallLocation::new(path.to_path_buf())])));
            }
            EventMsg::EnteredReviewMode(review_request) => {
                info!("Review begin: request={review_request:?}");
            }
            EventMsg::ExitedReviewMode(event) => {
                info!("Review end: output={event:?}");
                if let Err(err) = self.review_mode_exit(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::Warning(WarningEvent { message })
            | EventMsg::GuardianWarning(WarningEvent { message }) => {
                warn!("Warning: {message}");
                // ACP has no warning update kind. Carry the warning through the
                // standard text channel with an internal marker; Nori's client
                // turns it into its own history cell instead of merging it into
                // the following assistant response.
                client.send_agent_text(format!(
                    "{}{message}",
                    nori_protocol::AGENT_WARNING_PREFIX
                ));
            }
            EventMsg::McpStartupUpdate(McpStartupUpdateEvent { server, status }) => {
                info!("MCP startup update: server={server}, status={status:?}");
            }
            EventMsg::McpStartupComplete(McpStartupCompleteEvent {
                ready,
                failed,
                cancelled,
            }) => {
                info!(
                    "MCP startup complete: ready={ready:?}, failed={failed:?}, cancelled={cancelled:?}"
                );
            }
            EventMsg::ElicitationRequest(event) => {
                info!("Elicitation request: server={}, id={:?}", event.server_name, event.id);
                if let Err(err) = self.mcp_elicitation(client, event).await
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::ModelReroute(ModelRerouteEvent { from_model, to_model, reason }) => {
                info!("Model reroute: from={from_model}, to={to_model}, reason={reason:?}");
                self.model = Some(to_model.to_string());
            }
            EventMsg::ModelVerification(event) => {
                info!("Model verification requested: {event:?}");
            }

            EventMsg::ContextCompacted(..) => {
                info!("Context compacted");
                client.send_agent_text("Context compacted\n".to_string());
            }
            EventMsg::RequestPermissions(event) => {
                info!("Request permissions: {} {}", event.call_id, event.turn_id);
                if let Err(err) = self.request_permissions(client, event)
                    && let Some(response_tx) = self.response_tx.take()
                {
                    drop(response_tx.send(Err(err)));
                }
            }
            EventMsg::RequestUserInput(event) => {
                let params = json!({"sessionId": client.session_id, "callId": event.call_id, "turnId": event.turn_id, "questions": event.questions});
                let withdrawn = json!({"sessionId": client.session_id, "callId": event.call_id, "turnId": event.turn_id});
                let turn_id = event.turn_id;
                let client = client.clone();
                let thread = self.thread.clone();
                let cancellation = self.host_tool_cancellation.clone();
                drop(tokio::spawn(async move {
                    let answer = tokio::select! {
                        _ = cancellation.cancelled() => {
                            if let Err(error) = client.client.send_question_cancelled(withdrawn) {
                                warn!(%error, "failed to withdraw cancelled user question");
                            }
                            return;
                        },
                        answer = client.client.request_user_input(params) => answer,
                    };
                    if matches!(&answer, Ok(value) if value.get("cancelled").and_then(serde_json::Value::as_bool) == Some(true)) {
                        if let Err(error) = thread.submit(Op::Interrupt).await {
                            warn!(%error, "failed to cancel the user question");
                        }
                        return;
                    }
                    match answer.and_then(|value| serde_json::from_value::<codex_protocol::request_user_input::RequestUserInputResponse>(value).map_err(|error| Error::invalid_params().data(error.to_string()))) {
                        Ok(response) => {
                            if let Err(error) = thread.submit(Op::UserInputAnswer { id: turn_id, response }).await {
                                warn!(%error, "failed to submit user question response");
                            }
                        }
                        Err(error) => {
                            warn!(%error, "could not deliver user question; interrupting the turn");
                            client.send_agent_text("The user question could not be displayed; the turn was stopped.\n".to_owned());
                            drop(thread.submit(Op::Interrupt).await);
                        }
                    }
                }));
            }
            EventMsg::GuardianAssessment(event) => {
                info!(
                    "Guardian assessment: id={}, status={:?}, turn_id={}",
                    event.id, event.status, event.turn_id
                );
                self.guardian_assessment(client, event);
            }
            EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                call_id,
                new_thread_id: Some(child_session),
                new_agent_role,
                prompt,
                ..
            }) => {
                client.announce_subagent(
                    &call_id,
                    &child_session.to_string(),
                    new_agent_role.as_deref().unwrap_or("codex-subagent"),
                    &prompt,
                );
            }

            // Ignore these events
            EventMsg::AgentReasoningRawContent(..)
            | EventMsg::ThreadRolledBack(..)
            | EventMsg::HookStarted(..)
            | EventMsg::HookCompleted(..)
            // we already have a way to diff the turn, so ignore
            | EventMsg::TurnDiff(..)
            | EventMsg::ThreadSettingsApplied(..)
            // Old events
            | EventMsg::RawResponseItem(..)
            | EventMsg::SessionConfigured(..)
            | EventMsg::CollabAgentInteractionEnd(..)
            | EventMsg::RealtimeConversationStarted(..)
            | EventMsg::RealtimeConversationRealtime(..)
            | EventMsg::RealtimeConversationClosed(..)
            | EventMsg::RealtimeConversationSdp(..)
            | EventMsg::CollabWaitingBegin(..)
            | EventMsg::CollabWaitingEnd(..)
            | EventMsg::CollabResumeBegin(..)
            | EventMsg::CollabResumeEnd(..)
            | EventMsg::CollabCloseBegin(..)
            | EventMsg::CollabCloseEnd(..)
            | EventMsg::PlanDelta(..)=> {}
            e @ (EventMsg::RealtimeConversationListVoicesResponse(..)
            | EventMsg::DeprecationNotice(..)) => {
                warn!("Unexpected event: {:?}", e);
            }
                    other => {
                debug!("Ignoring event with no ACP equivalent: {other:?}");
            }
        }
    }

    pub(super) fn review_mode_exit(
        &self,
        client: &SessionClient,
        event: ExitedReviewModeEvent,
    ) -> Result<(), Error> {
        let ExitedReviewModeEvent { review_output, .. } = event;
        let Some(ReviewOutputEvent {
            findings,
            overall_correctness: _,
            overall_explanation,
            overall_confidence_score: _,
        }) = review_output
        else {
            return Ok(());
        };

        let text = if findings.is_empty() {
            let explanation = overall_explanation.trim();
            if explanation.is_empty() {
                "Reviewer failed to output a response"
            } else {
                explanation
            }
            .to_string()
        } else {
            format_review_findings_block(&findings, None)
        };

        client.send_agent_text(&text);
        Ok(())
    }
}

pub(super) fn format_thread_goal_update(event: &ThreadGoalUpdatedEvent) -> String {
    let status = match event.goal.status {
        ThreadGoalStatus::Active => "active",
        ThreadGoalStatus::Paused => "paused",
        ThreadGoalStatus::BudgetLimited => "budget limited",
        ThreadGoalStatus::Blocked => "blocked",
        ThreadGoalStatus::UsageLimited => "usage limited",
        ThreadGoalStatus::Complete => "complete",
    };

    let objective = event.goal.objective.trim();
    if objective.contains('\n') {
        format!("Goal updated ({status}):\n{objective}")
    } else {
        format!("Goal updated ({status}): {objective}")
    }
}
