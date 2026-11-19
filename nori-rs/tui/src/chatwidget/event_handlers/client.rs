//! Client events projected from ACP updates: the dispatch, replayed entries, approvals and plans.

use super::*;

impl ChatWidget {
    pub(in crate::chatwidget) fn on_plan_update(&mut self, update: PlanUpdate) {
        if self.plan_drawer_mode != PlanDrawerMode::Off {
            self.pinned_plan = Some(update);
            self.request_redraw();
        } else {
            self.add_to_history(history_cell::new_plan_update(
                update.clone(),
                self.pinned_plan.as_ref(),
            ));
            self.pinned_plan = Some(update);
        }
    }

    pub(crate) fn handle_client_event(&mut self, event: crate::presentation::ClientEvent) {
        match event {
            crate::presentation::ClientEvent::ApprovalRequest(_) => {}
            crate::presentation::ClientEvent::ToolSnapshot(tool_snapshot) => {
                self.note_agent_output();
                self.handle_client_tool_snapshot(tool_snapshot);
            }
            crate::presentation::ClientEvent::MessageDelta(message_delta) => {
                self.note_agent_output();
                self.handle_client_message_delta(message_delta);
            }
            crate::presentation::ClientEvent::PlanSnapshot(plan_snapshot) => {
                self.handle_client_plan_snapshot(plan_snapshot);
            }
            crate::presentation::ClientEvent::SessionPhaseChanged(phase) => {
                self.handle_client_phase_changed(phase);
            }
            crate::presentation::ClientEvent::PromptCompleted(completed) => {
                self.handle_client_prompt_completed(completed);
            }
            crate::presentation::ClientEvent::LoadCompleted => {
                self.request_redraw();
            }
            crate::presentation::ClientEvent::QueueChanged(queue_changed) => {
                self.main_queued_prompts = queue_changed.prompts;
                if self.focused_subagent.is_none() {
                    self.bottom_pane
                        .set_queued_user_messages(self.main_queued_prompts.clone());
                }
                self.request_redraw();
            }
            crate::presentation::ClientEvent::ContextCompacted(context_compacted) => {
                self.on_context_compacted(context_compacted.summary);
            }
            crate::presentation::ClientEvent::ReplayEntry(replay_entry) => {
                self.handle_client_replay_entry(replay_entry);
            }
            crate::presentation::ClientEvent::AgentCommandsUpdate(update) => {
                self.bottom_pane.set_agent_commands(update.commands);
            }
            crate::presentation::ClientEvent::SessionConfigUpdate(update) => {
                self.handle_acp_session_config_update(&update.config_options);
            }
            crate::presentation::ClientEvent::SessionUpdateInfo(update) => {
                if update.kind == crate::presentation::SessionUpdateKind::ConfigOptions {
                    self.refresh_acp_mode_config_snapshot();
                    self.request_redraw();
                    return;
                }
                self.clear_pending_goal_edit_if_no_goal(&update);
                if let Some(patch) = update.session_info_patch.as_ref() {
                    let origin = crate::nori::session_info::SessionInfoOrigin::from_replay_source(
                        self.replay_source,
                    );
                    self.session_info_state.apply(patch, origin);
                    self.publish_session_title();
                    if let Some(display) = crate::nori::session_info::display(
                        self.session_agent_info.as_ref(),
                        self.bottom_pane.agent_display_name(),
                        patch,
                        origin,
                        self.session_info_detail,
                    ) {
                        // Only an error is the transcript's business. A title or metadata update
                        // is the header's and the terminal title's: it is silent here.
                        if display.reports_error() {
                            self.add_side_cell(display.history_cell());
                        }
                        // An ordinary update is a toast in the top right, or nothing when
                        // toasts are off.
                        if !display.reports_error()
                            && self.config.toasts
                            && origin == crate::nori::session_info::SessionInfoOrigin::Live
                            && let Some(text) = display.toast_text()
                        {
                            self.app_event_tx.send(AppEvent::ShowToast(text));
                        }
                    }
                } else if update.kind == crate::presentation::SessionUpdateKind::Usage
                    && let Some(usage) = update.usage
                {
                    self.bottom_pane.set_session_usage(Some(usage));
                } else {
                    self.add_info_message(update.message, update.hint);
                }
                self.request_redraw();
            }
            crate::presentation::ClientEvent::SessionModeChanged(update) => {
                self.handle_acp_session_mode_changed(&update.current_mode_id);
            }
            crate::presentation::ClientEvent::SessionCapabilitiesChanged(update) => {
                self.handle_session_capabilities_changed(update);
            }
            crate::presentation::ClientEvent::ThreadGoalUpdated(update) => {
                self.handle_thread_goal_updated(update.goal);
            }
            crate::presentation::ClientEvent::ThreadGoalCleared => {
                self.handle_thread_goal_cleared();
            }
            crate::presentation::ClientEvent::Warning(warning) => {
                self.on_warning(warning.message);
            }
        }
    }

    pub(super) fn handle_client_plan_snapshot(
        &mut self,
        plan_snapshot: crate::presentation::PlanSnapshot,
    ) {
        self.on_plan_update(plan_snapshot_to_update_plan_args(plan_snapshot));
    }

    pub(super) fn handle_client_replay_entry(
        &mut self,
        replay_entry: crate::presentation::ReplayEntry,
    ) {
        match replay_entry {
            crate::presentation::ReplayEntry::UserMessage { text } => {
                self.add_to_history(history_cell::new_user_prompt(text));
            }
            crate::presentation::ReplayEntry::AssistantMessage { text } => {
                self.on_answer_text(&text);
                self.flush_answer_stream_with_separator();
            }
            crate::presentation::ReplayEntry::ReasoningMessage { text } => {
                let cell = history_cell::new_reasoning_summary_block(text);
                self.add_boxed_history(cell);
            }
            crate::presentation::ReplayEntry::PlanSnapshot { snapshot } => {
                self.add_to_history(history_cell::new_plan_update(
                    plan_snapshot_to_update_plan_args(snapshot),
                    None,
                ));
            }
            crate::presentation::ReplayEntry::ToolSnapshot { snapshot } => {
                self.handle_client_tool_snapshot(*snapshot);
            }
        }
        self.request_redraw();
    }

    pub(super) fn handle_client_approval_request(
        &mut self,
        approval: crate::presentation::ApprovalRequest,
        request_id: nori_protocol::acp::v1::RequestId,
        options: Vec<nori_protocol::acp::v1::PermissionOption>,
        subagent: Option<String>,
    ) {
        let Some(mut request) =
            approval_request_from_client_event(approval, &self.config.cwd, request_id, options)
        else {
            return;
        };
        request.offer_stop = self.cancel_ignored();
        // A subagent's request says whose it is.
        if let Some(label) = subagent {
            request.title = format!("[{label}] {}", request.title);
        }

        // The call's card, if it is still live, says it is waiting instead of spinning.
        let call_id = request.snapshot.call_id.clone();
        if let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<ClientToolCell>())
            .filter(|cell| cell.call_id() == call_id)
        {
            cell.set_awaiting_approval(true);
        } else if let Some(cell) = self.pending_client_tool_cells.get_mut(&call_id) {
            cell.set_awaiting_approval(true);
        } else if self.completed_client_tool_calls.contains(&call_id)
            || self.settled_client_tool_calls.contains(&call_id)
        {
            // Its card is in the transcript, still on screen: the proposal reaches it there.
            self.app_event_tx.send(AppEvent::UpdateHistoryToolSnapshot {
                generation: self.session_generation,
                snapshot: request.snapshot.clone(),
            });
        }
        self.flush_answer_stream_with_separator();
        self.notify(Notification::ExecApprovalRequested {
            command: request.title.clone(),
        });
        self.bottom_pane.push_approval_request(request);
        self.request_redraw();
    }
}

fn approval_request_from_client_event(
    approval: crate::presentation::ApprovalRequest,
    cwd: &std::path::Path,
    request_id: nori_protocol::acp::v1::RequestId,
    options: Vec<nori_protocol::acp::v1::PermissionOption>,
) -> Option<ApprovalRequest> {
    let crate::presentation::ApprovalSubject::ToolSnapshot(snapshot) = approval.subject;

    Some(ApprovalRequest {
        request_id,
        title: approval.title,
        kind: approval.kind,
        cwd: cwd.to_path_buf(),
        snapshot: Box::new(snapshot),
        options,
        offer_stop: false,
    })
}

fn plan_snapshot_to_update_plan_args(
    plan_snapshot: crate::presentation::PlanSnapshot,
) -> PlanUpdate {
    PlanUpdate {
        explanation: None,
        plan: plan_snapshot
            .entries
            .into_iter()
            .map(|entry| PlanItem {
                step: entry.step,
                status: match entry.status {
                    crate::presentation::PlanStatus::Pending => StepStatus::Pending,
                    crate::presentation::PlanStatus::InProgress => StepStatus::InProgress,
                    crate::presentation::PlanStatus::Completed => StepStatus::Completed,
                },
            })
            .collect(),
    }
}
