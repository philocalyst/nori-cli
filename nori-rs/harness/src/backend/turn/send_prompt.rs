//! Sending the local turn's `session/prompt` and wiring its outcome back into the ledger.

use super::super::*;

use crate::normalized::session_runtime::QueuedPromptKind;

use super::super::prompt_context::PromptContext;
use super::super::prompt_context::session::SessionStores;
use super::super::prompt_lifecycle::PromptResult;
use super::super::session_reducer::InboundEvent;

impl AcpBackend {
    /// Send the prompt of the local turn the reducer just opened. The request runs in a task
    /// whose result is delivered back to the ledger keyed by its wire request ID.
    pub(super) async fn send_prompt(
        &self,
        request_id: acp::RequestId,
        prompt: Vec<acp::ContentBlock>,
    ) {
        if let Some(abort_handle) = self.idle_timer_abort.lock().await.take() {
            abort_handle.abort();
        }

        let (
            prompt_kind,
            prompt_event_id,
            prompt_meta,
            original_content,
            admitted_context,
            display_text,
            turn_seq,
        ) = {
            let driver = self.session_driver.lock().await;
            let turn_seq = driver.local_turn_seq();
            driver
                .local_prompt()
                .map(|prompt| {
                    (
                        prompt.kind,
                        prompt.event_id.clone(),
                        prompt.prompt_meta.clone(),
                        prompt.original_content.clone(),
                        (*prompt.context).clone(),
                        prompt.display_text.clone(),
                        turn_seq,
                    )
                })
                .unwrap_or((
                    QueuedPromptKind::User,
                    request_id.to_string(),
                    None,
                    Vec::new(),
                    PromptContext::default(),
                    None,
                    turn_seq,
                ))
        };
        match prompt_kind {
            QueuedPromptKind::User => {
                self.edit_journal
                    .begin_turn(display_text.clone().unwrap_or_default());
                self.workflow_turn_started().await;
            }
            QueuedPromptKind::WorkflowHook => {
                self.edit_journal.begin_turn("workflow hook".to_string());
            }
            QueuedPromptKind::GoalContinuation => {
                self.edit_journal
                    .begin_turn("goal continuation".to_string());
            }
            QueuedPromptKind::LoopNudge => {
                self.edit_journal.begin_turn("loop nudge".to_string());
            }
            QueuedPromptKind::GateContinue => {
                self.edit_journal.begin_turn("gate continue".to_string());
            }
            QueuedPromptKind::TaskResults => {
                self.edit_journal
                    .begin_turn("delegated task results".to_string());
                if let Some(delivered) = self
                    .pending_task_results
                    .lock()
                    .await
                    .remove(&prompt_event_id)
                {
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            nori_protocol::NoriEvent::TaskResultsDelivered(delivered),
                        )))
                        .await;
                }
            }
            QueuedPromptKind::MonitorEvents => {
                self.edit_journal.begin_turn("monitor events".to_string());
            }
            QueuedPromptKind::Compact | QueuedPromptKind::NativeCompact => {}
        }
        // The turn just opened, to snapshot the tree for before its prompt is sent.
        let snapshot_turn = match prompt_kind {
            QueuedPromptKind::Compact | QueuedPromptKind::NativeCompact => None,
            QueuedPromptKind::User
            | QueuedPromptKind::WorkflowHook
            | QueuedPromptKind::GoalContinuation
            | QueuedPromptKind::LoopNudge
            | QueuedPromptKind::GateContinue
            | QueuedPromptKind::TaskResults
            | QueuedPromptKind::MonitorEvents => self.edit_journal.latest_turn_id(),
        };
        let client_request_started = self
            .pending_prompt_submissions
            .lock()
            .await
            .remove(&prompt_event_id);
        let (prompt_phase_tx, prompt_phase_rx) = tokio::sync::watch::channel(false);
        *self.prompt_phase_gate.lock().await = Some(prompt_phase_rx);
        let (transport_started_tx, transport_started_rx) = oneshot::channel();
        let (owner_tx, owner_rx) = oneshot::channel();
        let backend = (*self).clone();
        let prompt_result_tx = self.prompt_result_tx.clone();
        let request_id_for_task = request_id.clone();
        let prompt_meta_for_task = prompt_meta.clone();
        let original_content_for_task = original_content.clone();
        let prompt_task = tokio::spawn(async move {
            let session_id = backend.session_id.read().await.clone();
            if let Some(turn) = snapshot_turn {
                backend.edit_journal.capture_turn_start(turn).await;
            }
            // What the session owes the prompt that goes out (a compaction, fork or resume's
            // checkpoint, the preamble, hook output, goal context) is taken now, whichever
            // prompt that is, and given back below if the agent never receives it.
            let goals_enabled = backend.goal_mcp_http_server.lock().await.is_some();
            let stores = SessionStores {
                fixed: &backend.pending_static_context,
                hook_output: &backend.pending_hook_context,
                checkpoint: &backend.pending_compact_summary,
                workflow: &backend.workflow,
                goals: &backend.thread_goal_state,
                goals_enabled,
            };
            let context = stores
                .take(
                    prompt_kind,
                    admitted_context,
                    super::super::thread_goal::now_seconds(),
                )
                .await;
            let prompt = context.assemble(prompt);
            debug!(
                target: "acp_event_flow",
                request_id = %request_id_for_task,
                session_id = %session_id,
                ?prompt_kind,
                content_blocks = prompt.len(),
                "Sending ACP session/prompt request"
            );
            let original_meta = prompt_meta_for_task.clone();
            let super::super::prompt_recovery::PromptRun {
                wire_request_id,
                result,
                handoff,
            } = backend
                .prompt_with_recovery(
                    session_id,
                    prompt,
                    prompt_meta_for_task,
                    Some(transport_started_tx),
                    prompt_kind,
                    turn_seq,
                )
                .await;
            // Publish the reducer's final owner before delivering a result.
            // A failure before transport admission still belongs to the local ID.
            let owner_request_id = owner_rx
                .await
                .unwrap_or_else(|_| request_id_for_task.clone());
            match result {
                Ok(stop_reason) => {
                    debug!(
                        target: "acp_event_flow",
                        request_id = %wire_request_id,
                        ?stop_reason,
                        "Prompt task received ACP session/prompt response"
                    );
                    let _ = prompt_result_tx
                        .send(InboundEvent::PromptResultFor {
                            request_id: owner_request_id,
                            result: PromptResult::Response(stop_reason),
                        })
                        .await;
                }
                Err(err) => {
                    // The agent never took the prompt: the next one still owes it the context.
                    let mut context = context;
                    // Sent again as it was, a checkpoint the provider called too long would
                    // only be refused again.
                    if matches!(
                        nori_acp_host::classify_failure(&err).class,
                        nori_acp_host::FailureClass::PromptTooLong
                    ) && let Some(checkpoint) = context.dynamic.checkpoint.as_mut()
                        && let Some(smaller) = crate::context_budget::shrink_checkpoint(
                            checkpoint,
                            checkpoint.len() / 4,
                        )
                    {
                        *checkpoint = smaller;
                    }
                    stores.restore(context).await;
                    warn!(
                        target: "acp_event_flow",
                        request_id = %wire_request_id,
                        error = %err,
                        "The agent failed the prompt"
                    );
                    if let Some(mut handoff) = handoff {
                        // The conversation continues on another agent: this turn ends here
                        // without an error, and the prompt is sent again there.
                        handoff.prompt = Some(crate::recovery::handoff::HandoffPrompt {
                            content: original_content_for_task,
                            meta: original_meta,
                            request_id: wire_request_id.clone(),
                        });
                        let _ = prompt_result_tx
                            .send(InboundEvent::PromptResultFor {
                                request_id: owner_request_id,
                                result: PromptResult::Failed(
                                    crate::normalized::TurnFailure::Retryable,
                                ),
                            })
                            .await;
                        backend.request_handoff(handoff);
                        return;
                    }
                    let failure = backend
                        .send_prompt_error(&wire_request_id, prompt_kind, &err)
                        .await;
                    let _ = prompt_result_tx
                        .send(InboundEvent::PromptResultFor {
                            request_id: owner_request_id,
                            result: PromptResult::Failed(failure),
                        })
                        .await;
                }
            }
        });
        *self.prompt_task_abort.lock().await = Some(super::super::prompt_lifecycle::PromptTask {
            request_id: request_id.clone(),
            handle: prompt_task.abort_handle(),
        });

        let transport_start_result = match transport_started_rx.await {
            Ok(Ok(wire_request_id)) => {
                if let Some(task) = self
                    .prompt_task_abort
                    .lock()
                    .await
                    .as_mut()
                    .filter(|task| task.request_id == request_id)
                {
                    task.request_id = wire_request_id.clone();
                }
                let actions = self
                    .session_driver
                    .lock()
                    .await
                    .apply(InboundEvent::PromptStarted {
                        request_id: wire_request_id.clone(),
                    });
                debug_assert!(actions.side_effects.is_empty());
                debug_assert!(actions.completed_turn.is_none());
                self.forward_client_events(&actions.events).await;
                self.forward_observer_turn_status("working").await;
                Ok(wire_request_id)
            }
            Ok(Err(error)) => Err(error),
            Err(_) => Err(anyhow::anyhow!(
                "ACP transport did not assign a prompt request ID"
            )),
        };

        let owner = transport_start_result
            .as_ref()
            .unwrap_or(&request_id)
            .clone();
        let _ = owner_tx.send(owner);

        // The marker is the handed-over request's ID (or `true` from older callers).
        let handoff_retry = prompt_meta
            .as_ref()
            .and_then(|meta| meta.get(nori_protocol::HANDOFF_RETRY_META_KEY))
            .is_some_and(|marker| *marker != serde_json::Value::Bool(false));
        if prompt_kind == QueuedPromptKind::User && !handoff_retry {
            if let Some(recorder) = self.transcript_recorder.read().await.clone()
                && let Some(display_text) = display_text.as_deref()
                && let Err(error) = recorder
                    .record_user_message(&prompt_event_id, display_text, vec![])
                    .await
            {
                warn!("Failed to record user message to transcript: {error}");
            }

            let session_id = self.session_id.read().await.clone();
            let message_id = acp::MessageId::new(prompt_event_id);
            let echo_meta = prompt_meta.as_ref().and_then(|meta| {
                meta.get(nori_protocol::PROMPT_ECHO_ID_META_KEY)
                    .cloned()
                    .map(|echo_id| {
                        acp::Meta::from_iter([(
                            nori_protocol::PROMPT_ECHO_ID_META_KEY.to_string(),
                            echo_id,
                        )])
                    })
            });
            for content in original_content {
                let notification = acp::SessionNotification::new(
                    session_id.clone(),
                    acp::SessionUpdate::UserMessageChunk(
                        acp::ContentChunk::new(content)
                            .message_id(message_id.clone())
                            .meta(echo_meta.clone()),
                    ),
                );
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Acp(
                        nori_protocol::AcpEvent::Notification(
                            acp::AgentNotification::SessionNotification(notification),
                        ),
                    )))
                    .await;
            }
        }

        if let Some(request_started) = client_request_started {
            let _ = request_started.send(transport_start_result);
        }
        let _ = prompt_phase_tx.send(true);
    }
}
