//! What follows a local turn's completion: hooks, workflow and goal continuations, compaction,
//! and what follows any turn boundary.

use super::super::*;

use crate::normalized::ClientEvent;
use crate::normalized::session_runtime::QueuedPromptKind;

use super::super::session_runtime_driver::CompletedTurn;

impl AcpBackend {
    pub(super) async fn handle_completed_turn(
        &self,
        completed_turn: &CompletedTurn,
        generation: i64,
    ) {
        match completed_turn.prompt.kind {
            QueuedPromptKind::User
            | QueuedPromptKind::GoalContinuation
            | QueuedPromptKind::TaskResults
            | QueuedPromptKind::MonitorEvents
            | QueuedPromptKind::WorkflowHook
            | QueuedPromptKind::LoopNudge
            | QueuedPromptKind::GateContinue => {
                if let Some(last_agent_message) = &completed_turn.last_agent_message
                    && !last_agent_message.is_empty()
                    && !self.post_agent_response_hooks.is_empty()
                {
                    let env_vars = HashMap::from([
                        (
                            "NORI_HOOK_EVENT".to_string(),
                            "post_agent_response".to_string(),
                        ),
                        (
                            "NORI_HOOK_RESPONSE_TEXT".to_string(),
                            last_agent_message.clone(),
                        ),
                    ]);
                    let results = crate::hooks::execute_hooks_with_env(
                        &self.post_agent_response_hooks,
                        self.script_timeout,
                        &env_vars,
                    )
                    .await;
                    route_hook_results(&results, &self.backend_event_tx, None).await;
                }

                if let Some(last_agent_message) = &completed_turn.last_agent_message
                    && !last_agent_message.is_empty()
                    && !self.async_post_agent_response_hooks.is_empty()
                {
                    let env_vars = HashMap::from([
                        (
                            "NORI_HOOK_EVENT".to_string(),
                            "post_agent_response".to_string(),
                        ),
                        (
                            "NORI_HOOK_RESPONSE_TEXT".to_string(),
                            last_agent_message.clone(),
                        ),
                    ]);
                    let _ = crate::hooks::execute_hooks_fire_and_forget(
                        self.async_post_agent_response_hooks.clone(),
                        self.script_timeout,
                        env_vars,
                    );
                }

                if completed_turn.prompt.kind == QueuedPromptKind::User {
                    if let Some(display_text) = &completed_turn.prompt.display_text
                        && !self.post_user_prompt_hooks.is_empty()
                    {
                        let env_vars = HashMap::from([
                            (
                                "NORI_HOOK_EVENT".to_string(),
                                "post_user_prompt".to_string(),
                            ),
                            ("NORI_HOOK_PROMPT_TEXT".to_string(), display_text.clone()),
                        ]);
                        let results = crate::hooks::execute_hooks_with_env(
                            &self.post_user_prompt_hooks,
                            self.script_timeout,
                            &env_vars,
                        )
                        .await;
                        route_hook_results(
                            &results,
                            &self.backend_event_tx,
                            Some(&self.pending_hook_context),
                        )
                        .await;
                    }

                    if let Some(display_text) = &completed_turn.prompt.display_text
                        && !self.async_post_user_prompt_hooks.is_empty()
                    {
                        let env_vars = HashMap::from([
                            (
                                "NORI_HOOK_EVENT".to_string(),
                                "post_user_prompt".to_string(),
                            ),
                            ("NORI_HOOK_PROMPT_TEXT".to_string(), display_text.clone()),
                        ]);
                        let _ = crate::hooks::execute_hooks_fire_and_forget(
                            self.async_post_user_prompt_hooks.clone(),
                            self.script_timeout,
                            env_vars,
                        );
                    }
                }

                // A gate's say comes first: it is the person's own rule. Then the workflow's
                // follow-up, and the goal continues once that is done.
                self.gate_stop(completed_turn, generation).await;
                self.maybe_submit_workflow_hook(completed_turn, generation)
                    .await;
                self.maybe_submit_goal_continuation(completed_turn, generation)
                    .await;
            }
            QueuedPromptKind::Compact => {
                self.finish_compaction(completed_turn).await;
            }
            QueuedPromptKind::NativeCompact => {
                // Native compaction is an ordinary in-session turn: no session
                // swap and no summary re-injection. Emit the divider only if the agent
                // finished; a divider after a failed or cancelled compaction would claim the
                // context shrank.
                if completed_turn.stop_reason == acp::StopReason::EndTurn {
                    self.compaction_finished(super::super::compaction::CompactionOutcome::Swapped)
                        .await;
                    self.forward_client_event(ClientEvent::ContextCompacted(
                        crate::normalized::ContextCompacted { summary: None },
                    ))
                    .await;
                } else {
                    self.compaction_finished(super::super::compaction::CompactionOutcome::Failed)
                        .await;
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            nori_protocol::NoriEvent::Notice(nori_protocol::Notice {
                                message:
                                    "Compaction did not finish; the conversation goes on as it was."
                                        .to_string(),
                            }),
                        )))
                        .await;
                }
            }
        }
        self.on_turn_boundary(generation).await;
    }

    /// A turn of any owner ended. Results that arrived while it ran are handed over now, unless
    /// the turn queued another.
    pub(super) async fn on_turn_boundary(&self, generation: i64) {
        // Delivery waits for the runtime queue to admit its hidden prompt, so it must not wait
        // on that same queue from inside the runtime actor.
        let backend = self.clone();
        tokio::spawn(async move { backend.deliver_task_results_if_idle().await });
        self.monitor_turn_boundary();
        self.edit_journal.capture_turn_end();
        self.maybe_watch_ci(generation).await;
        // Last, after the turn's follow-ups had their chance to start: if the session is at
        // rest, its context is measured now, while the provider's cache is warm. This runs on
        // the runtime actor, which must never wait on its own queue: when that is full the
        // check is skipped, and the next prompt to start makes it instead.
        let _ =
            self.session_event_tx
                .try_send(session_runtime_driver::SessionRuntimeInput::Reducer(
                    session_reducer::InboundEvent::CompactIfDue,
                ));
    }

    async fn maybe_submit_goal_continuation(
        &self,
        completed_turn: &CompletedTurn,
        generation: i64,
    ) {
        if completed_turn.stop_reason != nori_protocol::acp::v1::StopReason::EndTurn {
            return;
        }

        // The agent's native goal loop owns continuation while the
        // `_session/goal` extension drives this goal.
        if self.thread_goal_state.lock().await.ext_driven() {
            return;
        }

        // Safety invariant: a hidden continuation may only chain after another
        // hidden continuation once we have observed the agent actually connect to
        // the `nori-client` MCP endpoint (`goal.connected`). Until then the agent
        // has no way to mark the goal complete/blocked, so unbounded
        // continuation-to-continuation chaining would loop forever. A user turn may
        // always start one continuation; only continuation-to-continuation chaining
        // requires the completion tool to be reachable.
        let can_chain_continuation = self
            .goal_mcp_connected
            .load(std::sync::atomic::Ordering::Relaxed);
        match completed_turn.prompt.kind {
            QueuedPromptKind::User
            | QueuedPromptKind::TaskResults
            | QueuedPromptKind::MonitorEvents
            | QueuedPromptKind::WorkflowHook
            | QueuedPromptKind::LoopNudge
            | QueuedPromptKind::GateContinue => {}
            QueuedPromptKind::GoalContinuation if can_chain_continuation => {}
            QueuedPromptKind::GoalContinuation
            | QueuedPromptKind::Compact
            | QueuedPromptKind::NativeCompact => return,
        }

        self.submit_goal_continuation_if_idle(generation).await;
    }

    pub(in crate::backend) async fn submit_goal_continuation_if_idle(&self, generation: i64) {
        if self.thread_goal_state.lock().await.ext_driven() {
            return;
        }
        if !self
            .goal_mcp_http_server
            .lock()
            .await
            .as_ref()
            .is_some_and(nori_client_mcp::NoriClient::has_goal_tools)
        {
            return;
        }

        if self.session_driver.lock().await.prompt_is_busy() {
            debug!(
                target: "acp_event_flow",
                "Skipping goal continuation because the ACP runtime is not idle"
            );
            return;
        }

        let prompt_text = self
            .thread_goal_state
            .lock()
            .await
            .continuation_prompt(thread_goal::now_seconds());
        let Some(prompt_text) = prompt_text else {
            return;
        };

        // Offered, not queued: if anything is running by now, the goal continues from the next
        // turn boundary instead.
        self.offer_prompt(
            Some(generation),
            crate::normalized::session_runtime::QueuedPrompt {
                event_id: format!("goal-continuation-{}", uuid::Uuid::new_v4()),
                kind: crate::normalized::session_runtime::QueuedPromptKind::GoalContinuation,
                text: prompt_text.clone(),
                prompt_meta: None,
                original_content: Vec::new(),
                content: vec![acp::ContentBlock::Text(acp::TextContent::new(prompt_text))],
                context: Default::default(),
                display_text: None,
            },
        )
        .await;
    }
}
