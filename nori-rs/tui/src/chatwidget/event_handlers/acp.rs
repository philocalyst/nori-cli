//! ACP events: notifications, permission requests and agent questions, and the proactive
//! (agent-owned) turns that begin without a request of ours.

use super::*;

impl ChatWidget {
    pub(super) fn handle_acp_event(&mut self, event: nori_protocol::AcpEvent) {
        match event {
            nori_protocol::AcpEvent::Notification(
                nori_protocol::acp::v1::AgentNotification::SessionNotification(notification),
            ) => {
                // Classify live proactive activity before projecting the same update for display.
                // Updates that belong to a subagent never reach the root's conversation, and the
                // echo of a prompt painted at submit is not painted twice.
                if self.route_root_update_to_subagents(&notification)
                    || self.is_painted_echo(&notification.update)
                {
                    return;
                }
                if self.replay_source.is_none() {
                    self.handle_proactive_session_update(&notification.update);
                } else {
                    self.note_replayed_update(&notification.update);
                }
                // A fresh call can reuse an id from a previous turn. Updates for settled calls
                // still target history, but a new announcement starts a new card.
                if let nori_protocol::acp::v1::SessionUpdate::ToolCall(call) = &notification.update
                {
                    self.settled_client_tool_calls
                        .remove(&call.tool_call_id.to_string());
                }
                let events = self
                    .client_event_normalizer
                    .push_session_update(&notification.update);
                for event in events {
                    if self.replay_source.is_some() {
                        self.handle_replay_client_event(event);
                    } else {
                        self.handle_client_event(event);
                    }
                }
            }
            nori_protocol::AcpEvent::ChildNotification(notification) => {
                self.handle_child_notification(&notification);
            }
            nori_protocol::AcpEvent::Request {
                request_id,
                request: nori_protocol::acp::v1::AgentRequest::ExtMethodRequest(request),
            } if request.method.as_ref() == "_nori/request_user_input" => {
                match serde_json::from_str::<crate::bottom_pane::UserQuestionRequest>(
                    request.params.get(),
                ) {
                    Ok(mut question) => {
                        if let Some(error) = question.validation_error() {
                            self.add_error_message(format!(
                                "Could not display agent question: {error}."
                            ));
                            if let Ok(raw) = serde_json::value::to_raw_value(
                                &serde_json::json!({"cancelled": true}),
                            ) {
                                self.app_event_tx.send(AppEvent::HarnessAction(crate::app_event::HarnessAction::RespondToAgent {
                                    request_id,
                                    response: Box::new(Ok(nori_protocol::acp::v1::ClientResponse::ExtMethodResponse(nori_protocol::acp::v1::ExtResponse::new(std::sync::Arc::from(raw))))),
                                }));
                            }
                            return;
                        }
                        question.label = self.subagent_label_for(&question.session_id);
                        question.request_id = Some(request_id);
                        self.bottom_pane.push_user_question(question);
                        self.app_event_tx.send(AppEvent::RevealUserQuestion);
                        self.request_redraw();
                    }
                    Err(error) => {
                        self.add_error_message(format!(
                            "Could not display agent question: {error}"
                        ));
                        if let Ok(raw) =
                            serde_json::value::to_raw_value(&serde_json::json!({"cancelled": true}))
                        {
                            self.app_event_tx.send(AppEvent::HarnessAction(
                                crate::app_event::HarnessAction::RespondToAgent {
                                    request_id,
                                    response: Box::new(Ok(
                                        nori_protocol::acp::v1::ClientResponse::ExtMethodResponse(
                                            nori_protocol::acp::v1::ExtResponse::new(
                                                std::sync::Arc::from(raw),
                                            ),
                                        ),
                                    )),
                                },
                            ));
                        }
                    }
                }
            }
            nori_protocol::AcpEvent::Request {
                request_id,
                request: nori_protocol::acp::v1::AgentRequest::RequestPermissionRequest(request),
            } => {
                let subagent = self.subagent_label_for(&request.session_id);
                for event in self
                    .client_event_normalizer
                    .push_permission_request(&request)
                {
                    if let crate::presentation::ClientEvent::ApprovalRequest(approval) = event {
                        self.handle_client_approval_request(
                            approval,
                            request_id.clone(),
                            request.options.clone(),
                            subagent.clone(),
                        );
                    }
                }
            }
            nori_protocol::AcpEvent::Response {
                response: Ok(nori_protocol::acp::v1::AgentResponse::InitializeResponse(response)),
                ..
            } => {
                self.follow_only_attachment = response
                    .meta
                    .as_ref()
                    .and_then(|meta| meta.get("nori"))
                    .and_then(|nori| nori.get("attachmentMode"))
                    .and_then(serde_json::Value::as_str)
                    == Some("follow");
                self.session_agent_info = response.agent_info;
                self.session_info_state.reset();
                self.publish_session_title();
                let capabilities = response.agent_capabilities;
                self.session_agent_capabilities = crate::presentation::AgentCapabilitiesView {
                    http_mcp: capabilities.mcp_capabilities.http,
                    load_session: capabilities.load_session,
                    session_list: capabilities.session_capabilities.list.is_some(),
                    session_resume: capabilities.session_capabilities.resume.is_some(),
                    session_close: capabilities.session_capabilities.close.is_some(),
                    session_fork: capabilities.session_capabilities.fork.is_some(),
                };
            }
            nori_protocol::AcpEvent::Response {
                request_id,
                response: Ok(nori_protocol::acp::v1::AgentResponse::PromptResponse(response)),
            } if self.owned_prompt_request_id.as_ref() == Some(&request_id) => {
                self.owned_prompt_request_id = None;
                self.handle_client_prompt_completed(crate::presentation::PromptCompleted {
                    stop_reason: response.stop_reason,
                    last_agent_message: None,
                    failure: None,
                });
            }
            nori_protocol::AcpEvent::Response {
                request_id,
                response: Err(error),
            } => {
                let message = Self::acp_error_message(&error);
                tracing::warn!(
                    code = i32::from(error.code),
                    message = %error.message,
                    detail = acp_error_detail(&error),
                    "ACP request failed"
                );
                if self.owned_prompt_request_id.as_ref() == Some(&request_id) {
                    if !self.unpaired_prompt_error_ids.remove(&request_id) {
                        self.unpaired_prompt_error_ids.insert(request_id);
                    }
                } else if !self.unpaired_prompt_error_ids.remove(&request_id) {
                    self.on_error(message);
                }
            }
            nori_protocol::AcpEvent::Request { .. }
            | nori_protocol::AcpEvent::Response { .. }
            | nori_protocol::AcpEvent::Notification(_) => {}
        }
    }

    pub(super) fn handle_proactive_session_update(
        &mut self,
        update: &nori_protocol::acp::v1::SessionUpdate,
    ) {
        // Known Nori statuses refine presentation only when no local request owns the turn.
        if let nori_protocol::acp::v1::SessionUpdate::SessionInfoUpdate(info) = update
            && let Some(status) = crate::presentation::nori_turn_status(info)
        {
            if self.locally_owned_request_active() {
                return;
            }
            match status {
                crate::presentation::NoriTurnStatus::Working => self.start_proactive_turn(),
                crate::presentation::NoriTurnStatus::Idle => self.complete_proactive_turn(),
            }
            return;
        }

        // Any unowned turn content is valid proactive activity, even without status hints.
        if !self.locally_owned_request_active() && is_turn_content_update(update) {
            self.start_proactive_turn();
        }
    }

    pub(super) fn locally_owned_request_active(&self) -> bool {
        self.owned_prompt_request_id.is_some()
            || matches!(
                self.acp_session_phase,
                Some(
                    crate::presentation::session_runtime::SessionPhaseView::Loading
                        | crate::presentation::session_runtime::SessionPhaseView::Prompt
                        | crate::presentation::session_runtime::SessionPhaseView::Cancelling
                )
            )
    }

    pub(super) fn start_proactive_turn(&mut self) {
        if !self.proactive_turn_active {
            self.proactive_turn_active = true;
            self.start_task_presentation(false);
        }
    }

    pub(super) fn separate_proactive_turn(&mut self) {
        self.record_proactive_assistant_message();
        self.proactive_turn_active = false;
        self.flush_answer_stream_with_separator();
    }

    pub(super) fn complete_proactive_turn(&mut self) {
        if self.proactive_turn_active {
            self.record_proactive_assistant_message();
            self.proactive_turn_active = false;
            self.finish_task_presentation(None, true);
        }
    }

    pub(super) fn record_proactive_assistant_message(&mut self) {
        if self.assistant_stream_seen_for_stats {
            self.session_stats.record_assistant_message();
        }
        self.assistant_stream_seen_for_stats = false;
    }
}

fn is_turn_content_update(update: &nori_protocol::acp::v1::SessionUpdate) -> bool {
    // Session metadata and capability changes do not by themselves begin a proactive turn.
    matches!(
        update,
        nori_protocol::acp::v1::SessionUpdate::UserMessageChunk(_)
            | nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(_)
            | nori_protocol::acp::v1::SessionUpdate::AgentThoughtChunk(_)
            | nori_protocol::acp::v1::SessionUpdate::Plan(_)
            | nori_protocol::acp::v1::SessionUpdate::ToolCall(_)
            | nori_protocol::acp::v1::SessionUpdate::ToolCallUpdate(_)
    )
}
