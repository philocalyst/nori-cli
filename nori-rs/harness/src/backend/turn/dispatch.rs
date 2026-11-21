//! Performing what the reducer decided: forwarding its events to clients and executing its side
//! effects.

use super::super::*;

use crate::normalized::ClientEvent;
use crate::normalized::session_runtime::QueuedPromptKind;

use super::super::session_reducer::InboundEvent;
use super::super::session_reducer::SideEffect;
use super::super::session_runtime_driver::ReducerActions;
use super::super::session_runtime_driver::SessionRuntimeInput;

fn client_event_kind(event: &ClientEvent) -> &'static str {
    match event {
        ClientEvent::SessionUpdateInfo(_) => "session_update_info",
        ClientEvent::SessionConfigUpdate(_) => "session_config_update",
        ClientEvent::SessionModeChanged(_) => "session_mode_changed",
        ClientEvent::SessionCapabilitiesChanged(_) => "session_capabilities_changed",
        ClientEvent::SessionPhaseChanged(_) => "session_phase_changed",
        ClientEvent::ToolsInterrupted(_) => "tools_interrupted",
        ClientEvent::QueueChanged(_) => "queue_changed",
        ClientEvent::MessageDelta(_) => "message_delta",
        ClientEvent::PromptCompleted(_) => "prompt_completed",
        ClientEvent::ToolSnapshot(_) => "tool_snapshot",
        ClientEvent::ApprovalRequest(_) => "approval_request",
        ClientEvent::AgentCommandsUpdate(_) => "agent_commands_update",
        ClientEvent::PlanSnapshot(_) => "plan_snapshot",
        ClientEvent::LoadCompleted => "load_completed",
        ClientEvent::ContextCompacted(_) => "context_compacted",
        ClientEvent::Warning(_) => "warning",
        ClientEvent::ReplayEntry(_) => "replay_entry",
        ClientEvent::ThreadGoalUpdated(_) => "thread_goal_updated",
        ClientEvent::ThreadGoalCleared => "thread_goal_cleared",
    }
}

impl AcpBackend {
    pub(in crate::backend) async fn apply_session_event(&self, event: InboundEvent) {
        let generation = self.orchestrator.admission_generation();
        let event_kind = session_reducer::inbound_event_kind(&event);
        let releases_fence = matches!(event, InboundEvent::ReleaseFence);
        let actions = {
            let mut driver = self.session_driver.lock().await;
            let phase_before = driver.phase_label();
            let active_before = driver.active_request_id();
            let queue_len_before = driver.queue_len();
            let actions = driver.apply(event);
            // One per streamed chunk: trace, so a long reply is not a long log.
            tracing::trace!(
                target: "acp_event_flow",
                event_kind,
                phase_before,
                active_request_id_before = active_before.as_deref().unwrap_or("<none>"),
                queue_len_before,
                phase_after = driver.phase_label(),
                active_request_id_after = driver.active_request_id().as_deref().unwrap_or("<none>"),
                queue_len_after = driver.queue_len(),
                client_events = actions.events.len(),
                side_effects = actions.side_effects.len(),
                "Applied reducer event in serialized session runtime"
            );
            actions
        };
        if let Some(completed) = &actions.completed_turn {
            // The request is over: its cancel watchdog and its task have nothing left to guard.
            if let Some(watchdog) = self
                .cancel_timeout_abort
                .lock()
                .await
                .take_if(|task| task.request_id == completed.request_id)
            {
                watchdog.abort();
            }
            drop(
                self.prompt_task_abort
                    .lock()
                    .await
                    .take_if(|task| task.request_id == completed.request_id),
            );
        }
        self.journal_queue_ops(&actions.queue_ops).await;
        let went_idle = actions.completed_turn.is_some() || actions.agent_ended.is_some();
        self.dispatch_reducer_actions(actions, generation).await;
        if releases_fence {
            // Whatever was owed while the swap held the fence (a wake, mail, a CI verdict)
            // was refused then; there may be no turn boundary left to offer it again.
            self.on_turn_boundary(generation).await;
        }
        if went_idle {
            self.maybe_start_idle_timer().await;
        }
    }

    pub(super) async fn dispatch_reducer_actions(&self, actions: ReducerActions, generation: i64) {
        let completed_prompt = actions.completed_turn.is_some();
        // A request the agent ignored a cancel for ends as a failure the user is told about,
        // before the session goes idle.
        for event in &actions.events {
            if let ClientEvent::PromptCompleted(completed) = event
                && completed.failure == Some(crate::normalized::TurnFailure::CancelIgnored)
            {
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::RequestFailed(nori_protocol::RequestFailure {
                            request_id: completed.request_id.clone(),
                            message: format!(
                                "The agent did not stop within {}s of the cancel request, so Nori \
                                 ended the request. If the agent is still working it shows as \
                                 running until it goes quiet; press Esc to send cancel again.",
                                prompt_lifecycle::CANCEL_FORCE.as_secs()
                            ),
                            kind: nori_protocol::RequestFailureKind::Fatal,
                        }),
                    )))
                    .await;
            }
        }
        match actions.completed_turn.as_ref().map(|turn| turn.prompt.kind) {
            // Summarize-and-swap compaction defers `PromptCompleted` until after
            // the session swap, so the new session is active before the UI sees
            // completion. Native compaction (below) does no swap and flows like
            // an ordinary turn, emitting its divider after completion.
            Some(QueuedPromptKind::Compact) => {
                let mut completion_event = None;
                let mut non_completion_events = Vec::new();
                for event in actions.events {
                    match event {
                        ClientEvent::PromptCompleted(_) => {
                            completion_event = Some(event);
                        }
                        other => non_completion_events.push(other),
                    }
                }
                self.forward_client_events(&non_completion_events).await;
                if let Some(completed_turn) = actions.completed_turn {
                    self.handle_completed_turn(&completed_turn, generation)
                        .await;
                }
                if let Some(event) = completion_event {
                    self.forward_client_event(event).await;
                }
            }
            _ => {
                self.forward_client_events(&actions.events).await;
                if let Some(completed_turn) = actions.completed_turn {
                    self.handle_completed_turn(&completed_turn, generation)
                        .await;
                }
            }
        }

        if completed_prompt || actions.agent_ended.is_some_and(|ended| ended.announce_idle) {
            self.forward_observer_turn_status("idle").await;
        }
        if let Some(ended) = actions.agent_ended {
            self.on_turn_boundary(generation).await;
            // An agent turn has no stop reason to say a goal should go on, so it continues only
            // when it was not interrupted and the agent can complete the goal itself.
            if !ended.interrupted
                && self
                    .goal_mcp_connected
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                self.submit_goal_continuation_if_idle(generation).await;
            }
        }

        for side_effect in actions.side_effects {
            self.execute_side_effect(side_effect).await;
        }
    }

    pub(super) async fn forward_client_events(&self, client_events: &[ClientEvent]) {
        for client_event in client_events {
            self.forward_client_event(client_event.clone()).await;
        }
    }

    /// Tell observers of the session whether it is working, in the Nori status metadata other
    /// frontends already understand.
    pub(super) async fn forward_observer_turn_status(&self, status: &str) {
        let mut meta = serde_json::Map::new();
        meta.insert("nori".to_string(), serde_json::json!({ "status": status }));
        let notification = acp::SessionNotification::new(
            self.session_id.read().await.clone(),
            acp::SessionUpdate::SessionInfoUpdate(acp::SessionInfoUpdate::new().meta(meta)),
        );
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Acp(
                nori_protocol::AcpEvent::Notification(acp::AgentNotification::SessionNotification(
                    notification,
                )),
            )))
            .await;
    }

    pub(in crate::backend) async fn forward_client_event(&self, client_event: ClientEvent) {
        match &client_event {
            ClientEvent::SessionPhaseChanged(phase) => {
                debug!(
                    target: "acp_event_flow",
                    client_event = client_event_kind(&client_event),
                    ?phase,
                    "Forwarding client event from ACP backend"
                );
            }
            ClientEvent::PromptCompleted(completed) => {
                debug!(
                    target: "acp_event_flow",
                    client_event = client_event_kind(&client_event),
                    stop_reason = ?completed.stop_reason,
                    has_last_agent_message = completed
                        .last_agent_message
                        .as_ref()
                        .is_some_and(|message| !message.is_empty()),
                    "Forwarding client event from ACP backend"
                );
            }
            _ => {
                // One per streamed chunk: trace, so a long reply is not a long log.
                tracing::trace!(
                    target: "acp_event_flow",
                    client_event = client_event_kind(&client_event),
                    "Forwarding client event from ACP backend"
                );
            }
        }
        if let ClientEvent::ToolSnapshot(snapshot) = &client_event {
            self.edit_journal.record(snapshot, &self.cwd, None);
        }
        let goal_event = self
            .thread_goal_update_from_client_event(&client_event)
            .await;
        let public_event = match &client_event {
            ClientEvent::SessionPhaseChanged(view) => {
                self.orchestrator.publish_root_phase(!matches!(
                    view,
                    crate::normalized::session_runtime::SessionPhaseView::Idle
                ));
                let phase = if matches!(
                    view,
                    crate::normalized::session_runtime::SessionPhaseView::Idle
                ) {
                    nori_protocol::SessionPhase::Idle
                } else {
                    self.session_driver.lock().await.public_phase()
                };
                Some(nori_protocol::NoriEvent::SessionPhaseChanged(phase))
            }
            ClientEvent::ToolsInterrupted(interrupted) => {
                // The agent never finished these calls. Close each one in the stream every
                // client and the journal reads, so none keeps showing as running.
                let session_id = self.session_id.read().await.clone();
                let meta = acp::Meta::from_iter([(
                    "nori".to_string(),
                    serde_json::json!({ "interrupted": interrupted.reason.as_str() }),
                )]);
                for call_id in &interrupted.call_ids {
                    let update = acp::SessionUpdate::ToolCallUpdate(
                        acp::ToolCallUpdate::new(
                            acp::ToolCallId::new(call_id.as_str()),
                            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Failed),
                        )
                        .meta(meta.clone()),
                    );
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Acp(
                            nori_protocol::AcpEvent::Notification(
                                acp::AgentNotification::SessionNotification(
                                    acp::SessionNotification::new(session_id.clone(), update),
                                ),
                            ),
                        )))
                        .await;
                }
                None
            }
            ClientEvent::QueueChanged(queue) => Some(nori_protocol::NoriEvent::QueueChanged(
                nori_protocol::QueueSnapshot {
                    prompts: queue.prompts.clone(),
                },
            )),
            ClientEvent::ContextCompacted(compacted) => Some(
                nori_protocol::NoriEvent::ContextCompacted(nori_protocol::ContextCompactedEvent {
                    summary: compacted.summary.clone(),
                }),
            ),
            ClientEvent::SessionCapabilitiesChanged(capabilities) => {
                Some(nori_protocol::NoriEvent::CapabilitiesChanged(
                    public_nori_capabilities(capabilities.clone()),
                ))
            }
            ClientEvent::ThreadGoalUpdated(update) => Some(nori_protocol::NoriEvent::GoalChanged(
                Some(update.goal.clone()),
            )),
            ClientEvent::ThreadGoalCleared => Some(nori_protocol::NoriEvent::GoalChanged(None)),
            ClientEvent::Warning(warning) => {
                Some(nori_protocol::NoriEvent::Notice(nori_protocol::Notice {
                    message: warning.message.clone(),
                }))
            }
            _ => None,
        };
        if let Some(event) = public_event {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(event)))
                .await;
        }
        if let Some(goal_event) = goal_event
            && let ClientEvent::ThreadGoalUpdated(update) = goal_event
        {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    nori_protocol::NoriEvent::GoalChanged(Some(update.goal)),
                )))
                .await;
        }
    }

    async fn execute_side_effect(&self, side_effect: SideEffect) {
        match side_effect {
            SideEffect::SendPrompt { request_id, prompt } => {
                // The ledger looks for loops, and measures the context, by the policy of the turn
                // that is starting.
                let policy = self.session_policy_now().await;
                self.session_driver
                    .lock()
                    .await
                    .set_policy(policy.loops, policy.compaction);
                self.send_prompt(request_id, prompt).await;
            }
            SideEffect::SendCancel {
                target,
                arm_watchdog,
            } => self.send_cancel(target, arm_watchdog).await,
            SideEffect::RejectPromptBusy { event_id } => {
                if let Some(response_tx) = self
                    .pending_prompt_submissions
                    .lock()
                    .await
                    .remove(&event_id)
                {
                    let _ = response_tx.send(Err(super::super::PromptBusy.into()));
                }
            }
            SideEffect::ResolvePermissionCancelled { request_id } => {
                self.resolve_cancelled_permission(&request_id).await;
            }
            SideEffect::AnnounceContinued {
                request_id,
                tool,
                cause,
                after_cancel,
            } => {
                // The wire ID is whatever the agent sent; find the request by its text.
                let wire_request_id = self
                    .pending_approvals
                    .lock()
                    .await
                    .iter()
                    .map(|pending| pending.request.request_id.clone())
                    .find(|wire| wire.to_string() == request_id);
                if let Some(wire_request_id) = wire_request_id {
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            nori_protocol::NoriEvent::AgentContinued(
                                nori_protocol::AgentContinued {
                                    agent: self.agent_name.clone(),
                                    request_id: wire_request_id,
                                    tool,
                                    cause,
                                    after_cancel,
                                },
                            ),
                        )))
                        .await;
                }
            }
            SideEffect::AnnounceDeclined { tool, count } => {
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::RequestDeclined(nori_protocol::RequestDeclined {
                            tool,
                            count,
                            agent: None,
                        }),
                    )))
                    .await;
            }
            SideEffect::AnnounceSilent { silent_for_secs } => {
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::AgentSilent(nori_protocol::AgentSilent {
                            silent_for_secs,
                        }),
                    )))
                    .await;
            }
            SideEffect::AnnounceHeard => {
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::AgentHeard,
                    )))
                    .await;
            }
            SideEffect::AnnounceRestored { prompts } => {
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::QueueRestored(nori_protocol::QueueRestored {
                            prompts: prompts
                                .into_iter()
                                .map(|(id, text)| nori_protocol::RestoredPrompt { id, text })
                                .collect(),
                        }),
                    )))
                    .await;
            }
            SideEffect::CompactFirst { usage, how } => self.compact_first(usage, how),
            SideEffect::PrepareSummary { usage } => self.prepare_summary(usage),
            SideEffect::Schedule { wake, after } => {
                let inputs = self.session_event_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let _ = inputs.send(SessionRuntimeInput::Wake(wake)).await;
                });
            }
        }
    }

    async fn maybe_start_idle_timer(&self) {
        if self.session_driver.lock().await.prompt_is_busy() {
            return;
        }

        let Some(duration) = self.notify_after_idle.as_duration() else {
            return;
        };

        let idle_secs = duration.as_secs();
        let user_notifier = Arc::clone(&self.user_notifier);
        let session_id = self.session_id.read().await.to_string();
        let idle_task = tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            user_notifier.notify(&crate::UserNotification::Idle {
                session_id,
                idle_duration_secs: idle_secs,
            });
        });
        *self.idle_timer_abort.lock().await = Some(idle_task.abort_handle());
    }
}
