//! Starting, closing and leaving sessions.

use super::*;

impl App {
    pub(super) async fn handle_session_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::NewSession => {
                if matches!(self.candidate_agent, Some(CandidateAgent::Prepared { .. })) {
                    let Some(CandidateAgent::Prepared {
                        agent_name,
                        display_name,
                        mut agent,
                        ..
                    }) = self.candidate_agent.take()
                    else {
                        unreachable!("candidate variant checked above")
                    };
                    let config = self.config_for_agent(&agent_name);
                    let mut prepare_spec = crate::chatwidget::agent::agent_prepare_spec(
                        config.clone(),
                        self.manual_model_handoff
                            .as_ref()
                            .map(|handoff| handoff.context.clone()),
                    );
                    prepare_spec.handoff = self
                        .manual_model_handoff
                        .as_ref()
                        .map(|handoff| handoff.recorder.clone());
                    if let Err(error) =
                        nori_harness::runtime::refresh_prepared_agent(&mut agent, prepare_spec)
                    {
                        tokio::spawn((*agent).shutdown());
                        self.pending_candidate_model = None;
                        if self.manual_model_handoff.is_some() {
                            let why = error.to_string();
                            if self.advance_model_route(&why).await {
                                return Ok(true);
                            }
                            self.end_model_choice(&why);
                            self.manual_model_handoff = None;
                            return Ok(true);
                        }
                        self.manual_model_handoff = None;
                        self.chat_widget.set_login_agent_override(Some(agent_name));
                        self.chat_widget.add_error_message(format!(
                            "Couldn't activate {display_name}: {error}"
                        ));
                        return Ok(true);
                    }
                    let mut init = self.chat_widget_init(
                        tui.frame_requester(),
                        None,
                        Vec::new(),
                        Some(config.active_agent.clone()),
                        false,
                        None,
                    );
                    init.config = config;
                    init.prepared_agent = Some(*agent);
                    let mut widget = ChatWidget::new_candidate(init);
                    if self.session_card_shown {
                        widget.suppress_welcome_card();
                    }
                    self.candidate_agent = Some(CandidateAgent::Activating {
                        agent_name,
                        widget: Box::new(widget),
                        activation_error: None,
                        activation_route_eligible: false,
                    });
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                self.discard_candidate();
                if let Some(preparation) = &mut self.primary_agent_preparation {
                    preparation.intent = crate::app_event::AgentPrepareIntent::Idle;
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    return Ok(true);
                }
                if self.prepared_agent.is_some() {
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    let agent = match self.take_refreshed_prepared_agent() {
                        Ok(Some(agent)) => agent,
                        Ok(None) => return Ok(true),
                        Err(error) => {
                            self.chat_widget.add_error_message(format!(
                                "Couldn't activate prepared agent: {error}"
                            ));
                            return Ok(true);
                        }
                    };
                    self.pending_session_activation = None;
                    self.deferred_spawn_pending = false;
                    let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                    let composer_draft = self.chat_widget.take_composer_draft();
                    let loop_state = self.chat_widget.loop_state();
                    self.shutdown_current_conversation();
                    let mut init = self.chat_widget_init(
                        tui.frame_requester(),
                        initial_prompt,
                        initial_images,
                        None,
                        false,
                        None,
                    );
                    init.prepared_agent = Some(agent);
                    self.chat_widget = ChatWidget::new(init);
                    self.configure_new_chat_widget();
                    self.chat_widget.restore_composer_draft(composer_draft);
                    if let Some((remaining, total)) = loop_state {
                        self.chat_widget.set_loop_state(remaining, total);
                    }
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                if !self.chat_widget.has_harness_session() {
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                    return Ok(true);
                }

                self.pending_session_activation = Some(PendingSessionActivation::New);
                self.deferred_spawn_pending = false;
                let summary = session_summary(
                    self.chat_widget.token_usage(),
                    self.chat_widget.conversation_id(),
                    self.chat_widget.session_stats().has_activity(),
                );
                let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                self.shutdown_current_conversation();
                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    initial_prompt,
                    initial_images,
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                if let Some(summary) = summary {
                    let mut lines: Vec<Line<'static>> = Vec::new();
                    if let Some(usage_line) = summary.usage_line {
                        lines.push(usage_line.into());
                    }
                    if let Some(command) = summary.resume_command {
                        lines.push(RESUME_HINT_LEAD.into());
                        lines.push(command.cyan().into());
                    }
                    self.chat_widget.add_plain_history_lines(lines);
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::SessionCloseFailed { message } => {
                self.chat_widget.on_session_close_failed(message);
            }
            AppEvent::SessionClosed => {
                self.program_status.clear();
                self.program_status.idle();
                // The session was released; land back on the session picker
                // with a fresh deferred widget — never auto-claim a new
                // session (on a cloud agent that would boot a new VM).
                self.cancel_primary_agent_preparation();
                self.discard_candidate();
                self.pending_session_activation = None;
                if let Some(agent) = self.prepared_agent.take() {
                    tokio::spawn(agent.shutdown());
                }
                self.prepared_agent_initial_context = None;
                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    None,
                    Vec::new(),
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.deferred_spawn_pending = true;
                self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Picker {
                    fallback_to_spawn: false,
                });
                tui.frame_requester().schedule_frame();
            }
            AppEvent::RemoteControlRequested(request) => {
                let tailnet = match request {
                    crate::remote_control::RemoteControlRequest::EnableLocal
                    | crate::remote_control::RemoteControlRequest::EnableTailnet
                    | crate::remote_control::RemoteControlRequest::Status => {
                        crate::remote_control::detect_tailnet_ipv4().await
                    }
                    crate::remote_control::RemoteControlRequest::EnableExplicit(_)
                    | crate::remote_control::RemoteControlRequest::Disable => {
                        Err("Tailscale was not queried.".to_string())
                    }
                };
                let outcome = self
                    .remote_control
                    .execute_request_with_detection(request, tailnet)
                    .await;
                match outcome {
                    crate::remote_control::RemoteControlOutcome::Report(report) => {
                        self.chat_widget.add_boxed_history(Box::new(
                            crate::history_cell::new_remote_control_event(report.lines()),
                        ));
                    }
                    crate::remote_control::RemoteControlOutcome::ConfirmationRequired(addr) => {
                        self.chat_widget.open_remote_control_confirmation(addr);
                    }
                    crate::remote_control::RemoteControlOutcome::Error(message) => {
                        self.chat_widget.add_error_message(message);
                    }
                }
            }
            AppEvent::ConfirmRemoteControlExplicit(addr) => {
                let outcome = self.remote_control.confirm_explicit(addr).await;
                match outcome {
                    crate::remote_control::RemoteControlOutcome::Report(report) => {
                        self.chat_widget.add_boxed_history(Box::new(
                            crate::history_cell::new_remote_control_event(report.lines()),
                        ));
                    }
                    crate::remote_control::RemoteControlOutcome::Error(message) => {
                        self.chat_widget.add_error_message(message);
                    }
                    crate::remote_control::RemoteControlOutcome::ConfirmationRequired(_) => {
                        unreachable!("a confirmed address cannot request confirmation again")
                    }
                }
            }
            AppEvent::OpenAgentSessionPicker => {
                self.discard_candidate();
                self.pending_session_activation = None;
                if let Some(agent) = &self.prepared_agent {
                    let can_resume_catalog = agent.capabilities().load_session
                        || agent.capabilities().session_capabilities.resume.is_some();
                    match agent.catalog() {
                        nori_harness::runtime::SessionCatalog::Listed(sessions)
                            if can_resume_catalog =>
                        {
                            self.chat_widget
                                .show_acp_resume_session_picker(sessions.clone(), false);
                        }
                        _ => self.chat_widget.open_local_resume_session_picker(),
                    }
                } else {
                    self.begin_agent_preparation(
                        crate::app_event::AgentPrepareIntent::ResumePicker,
                    );
                }
            }
            AppEvent::BeginExit => {
                self.cancel_primary_agent_preparation();
                if let Some(agent) = self.prepared_agent.take() {
                    tokio::spawn(agent.shutdown());
                }
                self.prepared_agent_initial_context = None;
                self.discard_candidate();
                self.chat_widget.begin_exit();
            }
            AppEvent::ExitRequest => {
                self.settle_for_exit();
                self.overlay = None;
                self.chat_widget.clear_main_history_view();
                self.cancel_primary_agent_preparation();
                if let Some(agent) = self.prepared_agent.take() {
                    agent.shutdown().await;
                }
                self.prepared_agent_initial_context = None;
                self.discard_candidate();
                // The exit message is the last row of the conversation.
                let exit_cell: Arc<dyn HistoryCell> =
                    self.chat_widget.create_exit_message_cell().into();
                self.transcript_cells.push(exit_cell);
                self.leave_screen(tui)?;

                // Exit the application
                return Ok(false);
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
