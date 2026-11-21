//! The life of a turn in the transcript: how it starts, how it ends, and what a failure looks like.

use super::*;
use crate::history_cell::NoticeKind;

impl ChatWidget {
    pub(in crate::chatwidget) fn set_status_header(&mut self, header: String) {
        self.current_status_header = header.clone();
        let suffix = self.subagent_status_suffix();
        self.bottom_pane
            .update_status_header(format!("{header}{suffix}"));
    }

    pub(in crate::chatwidget) fn on_context_compacted(&mut self, summary: Option<String>) {
        // Step 1: Flush the streamed summary from the old session.
        self.flush_answer_stream_with_separator();
        self.pending_client_tool_cells.clear();

        // Step 2: Show "Context compacted" as an info message.
        self.add_info_message("Context compacted".to_owned(), None);

        // When the ACP backend provides a summary, show a session header
        // followed by the summary reprinted as the first assistant message
        // of the new session. This makes the session boundary visible.
        if let Some(summary) = summary {
            // Step 3: Insert a new session header (same card as a fresh session,
            // but without install hints since this is not the first launch).
            use crate::nori::session_header::DisplayMode;
            use crate::nori::session_header::NoriSessionHeaderCell;
            let header = NoriSessionHeaderCell::new(self.live_status_view_model())
                .with_display_mode(DisplayMode::Compact);
            self.add_to_history(history_cell::SessionInfoCell::new(
                history_cell::CompositeHistoryCell::new(vec![Box::new(header)]),
            ));

            // Step 4: Reprint the summary as the first assistant message of the
            // new session.
            self.handle_streaming_delta(summary);
            self.flush_answer_stream_with_separator();
        }

        self.request_redraw();
    }

    // Raw reasoning uses the same flow as summarized reasoning

    pub(in crate::chatwidget) fn on_task_started(&mut self) {
        self.start_task_presentation(true);
    }

    pub(super) fn start_task_presentation(&mut self, owned: bool) {
        self.bottom_pane.clear_ctrl_c_quit_hint();
        // Owned requests enable task controls; proactive work gets display-only status.
        if owned {
            self.bottom_pane.set_task_running(true);
        } else {
            self.bottom_pane.ensure_status_indicator();
        }
        self.bottom_pane.set_interrupt_hint_visible(owned);
        self.set_status_header(crate::status_indicator_widget::pick_status_message(
            self.config.custom_working_messages,
            &self.config.custom_working_message_list,
        ));
        self.full_reasoning_buffer.clear();
        self.reasoning_buffer.clear();
        self.settle_client_tool_calls();
        self.assistant_stream_seen_for_stats = false;
        self.request_redraw();
        self.refresh_terminal_title();
    }

    pub(in crate::chatwidget) fn on_task_complete(&mut self, last_agent_message: Option<String>) {
        self.finish_task_presentation(last_agent_message, true);

        // Loop mode: if iterations remain, fire the next iteration.
        if let Some(remaining) = self.loop_remaining
            && remaining > 0
            && let Some(prompt) = self.first_prompt_text.clone()
        {
            let total = self.loop_total.unwrap_or(0);
            self.app_event_tx.send(AppEvent::LoopIteration {
                prompt,
                remaining: remaining - 1,
                total,
            });
        }
    }

    pub(super) fn finish_task_presentation(
        &mut self,
        last_agent_message: Option<String>,
        notify: bool,
    ) {
        // If a stream is currently active, finalize it. A burst too big to render in one frame
        // is finished by the frames that follow, as during the turn, instead of in one stall.
        self.commit_stream_for_frame();
        if self
            .stream_controller
            .as_ref()
            .is_some_and(StreamController::has_unrendered_input)
        {
            self.stream_ending = true;
            self.request_redraw();
        } else {
            self.flush_answer_stream_with_separator();
        }

        self.pending_client_tool_cells.clear();
        self.finalize_active_cell_as_failed();

        if !self.completed_client_tool_calls.is_empty() {
            self.app_event_tx.send(AppEvent::FinalizeHistoryTools {
                generation: self.session_generation,
                call_ids: self.completed_client_tool_calls.iter().cloned().collect(),
            });
        }

        // Mark task stopped and request redraw now that all content is in history.
        self.bottom_pane.set_task_running(false);
        self.settle_client_tool_calls();
        self.request_redraw();
        self.refresh_terminal_title();

        // Refresh system info (including git branch) on task completion.
        // This catches any branch changes that occurred during the agent's turn.
        self.app_event_tx
            .send(AppEvent::RefreshSystemInfoForDirectory {
                dir: self.config.cwd.clone(),
            });

        if notify {
            // Emit a notification when the turn completes (suppressed if focused).
            self.notify(Notification::AgentTurnComplete {
                response: last_agent_message.unwrap_or_default(),
            });
        }
    }

    /// Finalize any active tool as failed and stop/clear running UI state.
    /// A turn's committed calls are settled: remembered, but no longer the turn's to finalize.
    pub(in crate::chatwidget) fn settle_client_tool_calls(&mut self) {
        /// How many settled calls are remembered; the oldest ideas of a very long session go.
        const REMEMBERED: usize = 4096;
        if self.settled_client_tool_calls.len() > REMEMBERED {
            self.settled_client_tool_calls.clear();
        }
        self.settled_client_tool_calls
            .extend(self.completed_client_tool_calls.drain());
    }

    pub(in crate::chatwidget) fn finalize_turn(&mut self) {
        self.finalize_active_cell_as_failed();
        self.pending_client_tool_cells.clear();
        if !self.completed_client_tool_calls.is_empty() {
            self.app_event_tx.send(AppEvent::FinalizeHistoryTools {
                generation: self.session_generation,
                call_ids: self.completed_client_tool_calls.iter().cloned().collect(),
            });
        }
        self.bottom_pane.set_task_running(false);
        self.settle_client_tool_calls();
        self.stream_controller = None;
        self.stream_ending = false;
        self.live_tail.set(None);
        self.commit_deferred_cells();
    }

    pub(in crate::chatwidget) fn on_error(&mut self, message: String) {
        // Display only. Loop lifecycle is owned by the prompt completion
        // (`handle_client_prompt_completed`), which carries the failure
        // disposition; deciding it here too would race across channels.
        self.finalize_turn();
        self.push_notice(NoticeKind::Error, message);
    }

    pub(in crate::chatwidget) fn on_warning(&mut self, message: impl Into<String>) {
        self.push_notice(NoticeKind::Warning, message.into());
    }

    /// Handle Ctrl-C key press.
    pub(in crate::chatwidget) fn on_ctrl_c(&mut self) {
        // Ctrl+C bypasses BottomPane key routing, so gate it here: once exit
        // is in progress it must not interrupt, clear, or re-hint anything.
        if self.exiting {
            return;
        }
        if self.bottom_pane.on_ctrl_c() == CancellationEvent::Handled {
            return;
        }

        if self.bottom_pane.is_task_running() {
            self.bottom_pane.show_ctrl_c_quit_hint();
            self.submit_harness_action(crate::app_event::HarnessAction::Cancel);
            return;
        }

        self.begin_exit();
    }

    pub(super) fn handle_client_phase_changed(
        &mut self,
        phase: crate::presentation::session_runtime::SessionPhaseView,
    ) {
        let previous_phase = self.acp_session_phase.replace(phase);

        match phase {
            crate::presentation::session_runtime::SessionPhaseView::Idle => {
                // What a running turn said in the status line ends with it (a route stays).
                if matches!(
                    self.bottom_pane.status_note(),
                    Some(
                        crate::bottom_pane::StatusNote::Retry { .. }
                            | crate::bottom_pane::StatusNote::Detached
                            | crate::bottom_pane::StatusNote::CancelIgnored
                            | crate::bottom_pane::StatusNote::AgentSilent { .. }
                    )
                ) {
                    self.bottom_pane.set_status_note(None);
                }
                self.bottom_pane.set_task_running(false);
                self.bottom_pane.set_interrupt_hint_visible(false);
                if !matches!(
                    previous_phase,
                    Some(crate::presentation::session_runtime::SessionPhaseView::Idle)
                ) {
                    self.request_redraw();
                    self.refresh_terminal_title();
                }
            }
            crate::presentation::session_runtime::SessionPhaseView::Loading => {
                self.bottom_pane.set_task_running(true);
                self.bottom_pane.ensure_status_indicator();
                self.bottom_pane.set_interrupt_hint_visible(false);
                self.set_status_header("Loading session".to_string());
                self.request_redraw();
                self.refresh_terminal_title();
            }
            crate::presentation::session_runtime::SessionPhaseView::Prompt => {
                if matches!(
                    previous_phase,
                    Some(crate::presentation::session_runtime::SessionPhaseView::Prompt)
                        | Some(crate::presentation::session_runtime::SessionPhaseView::Cancelling)
                ) {
                    self.bottom_pane.set_task_running(true);
                    self.bottom_pane.ensure_status_indicator();
                    self.bottom_pane.set_interrupt_hint_visible(true);
                    self.request_redraw();
                    self.refresh_terminal_title();
                } else {
                    self.on_task_started();
                }
            }
            crate::presentation::session_runtime::SessionPhaseView::Cancelling => {
                self.bottom_pane.set_task_running(true);
                self.bottom_pane.ensure_status_indicator();
                self.bottom_pane.set_interrupt_hint_visible(false);
                self.set_status_header("Cancelling".to_string());
                self.request_redraw();
                self.refresh_terminal_title();
            }
        }
    }

    pub(super) fn handle_client_prompt_completed(
        &mut self,
        completed: crate::presentation::PromptCompleted,
    ) {
        // The completion owns the loop lifecycle: a fatal failure disarms the
        // loop *before* on_task_complete can re-fire it, while a retryable
        // failure leaves it armed so the next iteration retries. Deciding this
        // here (rather than in on_error) keeps it on a single ordered event.
        if completed.failure == Some(crate::presentation::TurnFailure::Fatal) {
            self.cancel_loop();
        }
        // A failure already surfaces its own error cell; only a clean user
        // cancellation shows the generic "interrupted" notice.
        let interrupted = completed.stop_reason == nori_protocol::acp::v1::StopReason::Cancelled
            && completed.failure.is_none();
        let has_final_message = completed
            .last_agent_message
            .as_ref()
            .is_some_and(|message| !message.is_empty());
        if has_final_message || self.assistant_stream_seen_for_stats {
            self.session_stats.record_assistant_message();
        }
        self.assistant_stream_seen_for_stats = false;
        self.pending_echoes.clear();
        self.subagents_turn_ended(interrupted);
        self.on_task_complete(completed.last_agent_message);
        if interrupted {
            self.push_notice_with_hint(
                NoticeKind::Info,
                "Interrupted".to_owned(),
                Some("tell the model what to do differently".to_owned()),
            );
        }
        self.note_turn_end(completed.stop_reason, completed.failure.is_some());
    }
}
