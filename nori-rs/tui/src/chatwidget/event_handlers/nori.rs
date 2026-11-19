//! Nori events: session lifecycle, queues, goals, subagent wires, shell and notices.

use super::*;
use crate::history_cell::NoticeKind;

impl ChatWidget {
    pub(super) fn handle_nori_event(&mut self, event: nori_protocol::NoriEvent) {
        match event {
            nori_protocol::NoriEvent::ConversationResponseBoundary(boundary) => {
                if self.flush_answer_stream_with_separator() {
                    self.app_event_tx.send(AppEvent::ResponseBoundary(boundary));
                }
            }
            nori_protocol::NoriEvent::SessionStarted(started) => {
                self.on_session_started(started);
            }
            nori_protocol::NoriEvent::SessionPhaseChanged(phase) => {
                // A local request boundary separates statusless proactive output without completing it.
                if self.proactive_turn_active
                    && matches!(
                        phase,
                        nori_protocol::SessionPhase::Loading { .. }
                            | nori_protocol::SessionPhase::Prompting { .. }
                    )
                {
                    self.separate_proactive_turn();
                }
                let phase = match phase {
                    nori_protocol::SessionPhase::Idle => {
                        crate::presentation::session_runtime::SessionPhaseView::Idle
                    }
                    nori_protocol::SessionPhase::Loading { .. } => {
                        crate::presentation::session_runtime::SessionPhaseView::Loading
                    }
                    nori_protocol::SessionPhase::Prompting { request_id } => {
                        self.owned_prompt_request_id = Some(request_id);
                        crate::presentation::session_runtime::SessionPhaseView::Prompt
                    }
                    nori_protocol::SessionPhase::Cancelling { request_id } => {
                        self.owned_prompt_request_id = Some(request_id);
                        crate::presentation::session_runtime::SessionPhaseView::Cancelling
                    }
                    // The agent is working without a request of ours: running, and Esc cancels it.
                    nori_protocol::SessionPhase::Agent {
                        cause,
                        cancelling: false,
                    } => {
                        self.on_agent_turn_started(cause);
                        crate::presentation::session_runtime::SessionPhaseView::Prompt
                    }
                    nori_protocol::SessionPhase::Agent {
                        cancelling: true, ..
                    } => crate::presentation::session_runtime::SessionPhaseView::Cancelling,
                };
                self.handle_client_phase_changed(phase);
            }
            nori_protocol::NoriEvent::SessionEnded(ended) => {
                if !self.session_configured_received {
                    self.bottom_pane.hide_status_indicator();
                }
                match ended.reason {
                    nori_protocol::SessionEndReason::Shutdown => {
                        self.app_event_tx.send(AppEvent::ExitRequest);
                    }
                    nori_protocol::SessionEndReason::Closed => {
                        self.app_event_tx.send(AppEvent::SessionClosed);
                    }
                    nori_protocol::SessionEndReason::ConnectionLost
                    | nori_protocol::SessionEndReason::SpawnFailed
                    | nori_protocol::SessionEndReason::TimedOut => {}
                }
            }
            nori_protocol::NoriEvent::QueueChanged(queue) => {
                self.main_queued_prompts = queue.prompts;
                if self.focused_subagent.is_none() {
                    self.bottom_pane
                        .set_queued_user_messages(self.main_queued_prompts.clone());
                }
                self.request_redraw();
            }
            nori_protocol::NoriEvent::SubagentQueueChanged {
                session_id,
                prompts,
            } => {
                self.child_queued_prompts
                    .insert(session_id.clone(), prompts.clone());
                if self
                    .focused_subagent
                    .and_then(|key| self.subagent_session(key))
                    .as_ref()
                    == Some(&session_id)
                {
                    self.bottom_pane.set_queued_user_messages(prompts);
                }
                self.request_redraw();
            }
            nori_protocol::NoriEvent::ContextCompacted(compacted) => {
                self.on_context_compacted(compacted.summary);
            }
            nori_protocol::NoriEvent::GoalChanged(Some(goal)) => {
                self.handle_thread_goal_updated(goal);
            }
            nori_protocol::NoriEvent::GoalChanged(None) => self.handle_thread_goal_cleared(),
            nori_protocol::NoriEvent::CapabilitiesChanged(capabilities) => {
                self.handle_session_capabilities_changed(
                    crate::presentation::SessionCapabilitiesView {
                        agent: self.session_agent_capabilities.clone(),
                        nori_client: Default::default(),
                        builtin_commands: capabilities.builtin_commands,
                    },
                );
            }
            nori_protocol::NoriEvent::PromptSummaryUpdated(summary) => {
                self.on_prompt_summary(summary.summary);
            }
            nori_protocol::NoriEvent::Notice(notice) => self.on_warning(notice.message),
            nori_protocol::NoriEvent::AgentContinued(continued) => {
                // An agent-owned turn is already said once where it begins; what is worth a
                // line is an agent that went on after the user cancelled.
                if continued.after_cancel {
                    self.set_status_note(Some(crate::bottom_pane::StatusNote::CancelIgnored));
                    self.push_notice_with_hint(
                        NoticeKind::Warning,
                        format!("{} kept working after you cancelled", continued.agent),
                        Some(format!("asking to {}", continued.tool)),
                    );
                }
            }
            nori_protocol::NoriEvent::PromptUnanswered(unanswered) => {
                // Only in a replay: the session ended before the agent answered this prompt.
                self.push_notice_with_hint(
                    NoticeKind::Warning,
                    "The agent never answered your last prompt".to_owned(),
                    Some(crate::text_formatting::truncate_text(
                        unanswered.text.trim(),
                        60,
                    )),
                );
            }
            nori_protocol::NoriEvent::QueueRestored(restored) => {
                // The restored prompts themselves are the queue the composer shows.
                if !restored.prompts.is_empty() {
                    self.push_notice(
                        NoticeKind::Info,
                        match restored.prompts.len() {
                            1 => "1 prompt typed ahead was restored".to_owned(),
                            count => format!("{count} prompts typed ahead were restored"),
                        },
                    );
                }
            }
            nori_protocol::NoriEvent::AgentSilent(silent) => {
                // Not a failure: a long silent tool and a lost response look the same. The
                // status line says so; the transcript has nothing to add.
                self.set_status_note(Some(crate::bottom_pane::StatusNote::AgentSilent {
                    secs: silent.silent_for_secs,
                    at: std::time::Instant::now(),
                }));
            }
            nori_protocol::NoriEvent::AgentHeard => {
                if matches!(
                    self.bottom_pane.status_note(),
                    Some(crate::bottom_pane::StatusNote::AgentSilent { .. })
                ) {
                    self.set_status_note(None);
                }
            }
            nori_protocol::NoriEvent::RequestDeclined(declined) => {
                // The count grows while the agent keeps asking; the first says it.
                if declined.count == 1 {
                    self.push_notice_with_hint(
                        NoticeKind::Info,
                        format!("Declined {} after you cancelled", declined.tool),
                        Some("the agent kept asking".to_owned()),
                    );
                }
            }
            nori_protocol::NoriEvent::RouteChanged(changed) => self.on_route_changed(&changed),
            nori_protocol::NoriEvent::RetryScheduled(retry) => self.on_retry_scheduled(&retry),
            nori_protocol::NoriEvent::AttemptSuperseded(superseded) => {
                self.on_attempt_superseded(superseded.by)
            }
            nori_protocol::NoriEvent::SubscriptionExhausted(_) => {}
            nori_protocol::NoriEvent::RequestWithdrawn(request_id) => {
                self.bottom_pane.withdraw_approval_request(&request_id);
            }
            nori_protocol::NoriEvent::TaskResultsDelivered(delivered) => {
                // The results open the turn they wake, so whatever answer is still open is whole
                // now and the marker goes before the answer it introduces, never after it.
                self.flush_answer_stream_with_separator();
                // One quiet line per result, from whom and about what.
                for task in delivered.tasks {
                    self.add_info_message(
                        format!("Result from {}", task.agent.to_lowercase()),
                        Some(task.title),
                    );
                }
            }
            nori_protocol::NoriEvent::Monitor(wire) => self.on_monitor(wire),
            nori_protocol::NoriEvent::RequestFailed(failure) => {
                if !self.session_configured_received {
                    self.bottom_pane.hide_status_indicator();
                }
                let completes_active_prompt =
                    failure.request_id.as_ref().is_some_and(|request_id| {
                        self.owned_prompt_request_id.as_ref() == Some(request_id)
                    });
                if completes_active_prompt
                    && let Some(request_id) = failure.request_id.as_ref()
                    && !self.unpaired_prompt_error_ids.remove(request_id)
                {
                    self.unpaired_prompt_error_ids.insert(request_id.clone());
                }
                if completes_active_prompt {
                    self.on_error(failure.message);
                    self.owned_prompt_request_id = None;
                    let failure = match failure.kind {
                        nori_protocol::RequestFailureKind::Retryable => {
                            crate::presentation::TurnFailure::Retryable
                        }
                        nori_protocol::RequestFailureKind::Fatal => {
                            crate::presentation::TurnFailure::Fatal
                        }
                    };
                    self.handle_client_prompt_completed(crate::presentation::PromptCompleted {
                        stop_reason: nori_protocol::acp::v1::StopReason::Cancelled,
                        last_agent_message: None,
                        failure: Some(failure),
                    });
                } else {
                    self.add_error_message(failure.message);
                }
            }
            nori_protocol::NoriEvent::HookOutput(output) => match output.level {
                nori_protocol::HookOutputLevel::Info => self.add_info_message(output.message, None),
                nori_protocol::HookOutputLevel::Warn => self.on_warning(output.message),
                nori_protocol::HookOutputLevel::Error => self.on_error(output.message),
            },
            nori_protocol::NoriEvent::ReplayStarted(started) => {
                self.flush_answer_stream_with_separator();
                self.replay_source = Some(started.source);
                self.app_event_tx.send(AppEvent::HistoryReplay(
                    crate::app_event::HistoryReplay::Started(started.source),
                ));
            }
            nori_protocol::NoriEvent::ReplayFinished => {
                self.end_replayed_turn();
                self.replay_source = None;
                self.app_event_tx.send(AppEvent::HistoryReplay(
                    crate::app_event::HistoryReplay::Finished,
                ));
            }
            nori_protocol::NoriEvent::Subagent(wire) => self.handle_subagent_wire(wire),
            nori_protocol::NoriEvent::UserShell(shell_event) => {
                self.on_user_shell_event(shell_event);
            }
            nori_protocol::NoriEvent::Undo(_) | nori_protocol::NoriEvent::SessionReady(_) => {}
            // TODO(#557): render forked-transcript lineage in the TUI history.
            nori_protocol::NoriEvent::SessionForked(forked) => {
                self.on_session_forked(forked);
            }
        }
    }

    pub(super) fn on_session_forked(&mut self, forked: nori_protocol::SessionForked) {
        // The active conversation is now the forked child; the parent stays
        // resumable and is surfaced as `forked from:` on the status card.
        self.conversation_id =
            nori_harness::ConversationId::from_string(&forked.new_conversation_id).ok();
        self.forked_from =
            nori_harness::ConversationId::from_string(&forked.previous_conversation_id).ok();

        let hint = self.forked_from.as_ref().map(|previous| {
            format!(
                "resume the original with {}",
                crate::resume_command_for_conversation(previous)
            )
        });
        self.push_notice_with_hint(NoticeKind::Info, "Session forked".to_owned(), hint);
    }

    pub(super) fn on_session_started(&mut self, event: nori_protocol::SessionStarted) {
        self.session_configured_received = true;
        self.submit_harness_action(crate::app_event::HarnessAction::LoadCustomPrompts);
        self.bottom_pane.hide_status_indicator();
        self.update_approval_mode_label();
        self.bottom_pane.set_history_metadata(
            u64::try_from(event.history_log_id).unwrap_or_default(),
            usize::try_from(event.history_entry_count).unwrap_or_default(),
        );
        self.subagents.set_root(event.acp_session_id.clone());
        self.conversation_id = event
            .transcript_id
            .as_deref()
            .and_then(|id| nori_harness::ConversationId::from_string(id).ok());
        self.current_rollout_path = event.transcript_path;
        self.acp_session_id = self.cloud_mode.then(|| event.acp_session_id.to_string());
        self.refresh_cloud_session_indicator();
        // Adopt the configuration the agent advertised when it created the
        // session. When the welcome card is still pending — the agent-switch
        // candidate, which stays hidden until this event — adopt it silently
        // first so the card below names the agent's actual model and options
        // instead of the provider alone. When the card was already written at
        // startup it cannot gain the model after the fact, because history is
        // committed to the scrollback as it is written, so announce the
        // resolved configuration on its own line instead.
        if event.swap.is_some() {
            // The harness replaced the agent under this conversation. The route change is the
            // one marker the user sees; the header and status update in place, with no card.
            if !event.config_options.is_empty() {
                self.sync_acp_session_config_snapshot(&event.config_options);
            }
            self.refresh_acp_mode_config_snapshot();
            self.refresh_terminal_title();
            self.request_redraw();
            return;
        }
        if !event.config_options.is_empty() {
            if self.show_welcome_banner {
                self.sync_acp_session_config_snapshot(&event.config_options);
            } else {
                self.handle_acp_session_config_update(&event.config_options);
            }
        }
        let show_welcome_banner = std::mem::take(&mut self.show_welcome_banner);
        // A prompt painted before the session started stays below the session
        // header it was typed ahead of.
        let painted_prompt = self
            .active_cell
            .take_if(|cell| cell.as_any().is::<history_cell::UserHistoryCell>());
        self.add_to_history(history_cell::new_session_info(
            &self.config,
            self.config.active_agent.clone(),
            show_welcome_banner,
            self.live_status_view_model(),
        ));
        if painted_prompt.is_some() {
            self.active_cell.set(painted_prompt);
        }
        if let Some(user_message) = self.initial_user_message.take() {
            self.bottom_pane
                .record_local_history_submission(&user_message.text);
            self.submit_user_message(user_message);
        }
        if !self.suppress_session_configured_redraw {
            self.request_redraw();
        }
        self.refresh_acp_mode_config_snapshot();
        self.refresh_terminal_title();
    }
}
