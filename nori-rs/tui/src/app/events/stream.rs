//! The session's event stream, as the UI follows it.

use super::*;

impl App {
    pub(super) async fn handle_stream_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::SessionEvent { generation, event } => {
                if self.chat_widget.session_generation() == generation {
                    self.program_status.event(&event);
                }
                if self.chat_widget.session_generation() == generation
                    && let nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionMoveRequested { machine },
                    ) = &event
                {
                    self.begin_move(machine.clone());
                }
                if self.pending_move.is_some() {
                    self.start_pending_move();
                }
                if self.chat_widget.session_generation() == generation
                    && let nori_protocol::SessionEvent::Acp(
                        nori_protocol::AcpEvent::ChildNotification(notification),
                    ) = &event
                    && matches!(
                        &notification.update,
                        nori_protocol::acp::v1::SessionUpdate::ConfigOptionUpdate(_)
                    )
                    && let crate::subagent_switcher::Target::Subagent(key) =
                        self.session_view.focused()
                    && self.chat_widget.subagent_session(key).as_ref()
                        == Some(&notification.session_id)
                {
                    self.refresh_subagent_controls(key);
                }
                if self.chat_widget.session_generation() == generation
                    && let nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionStarted(started),
                    ) = &event
                {
                    self.active_transcript_id = started.transcript_id.clone();
                }
                let candidate_matches = matches!(
                    &self.candidate_agent,
                    Some(CandidateAgent::Activating { widget, .. })
                        if widget.session_generation() == generation
                );
                if candidate_matches {
                    // The harness replaced the session's agent: the app's notion of the active
                    // agent follows, before the new session's own events are handled.
                    if let nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::RouteChanged(changed),
                    ) = &event
                        && self.chat_widget.session_generation() == generation
                        && changed.to.agent != self.config.active_agent
                    {
                        self.config.active_agent = changed.to.agent.clone();
                        self.config.agent = changed.to.agent.clone();
                    }
                    let started = match &event {
                        nori_protocol::SessionEvent::Nori(
                            nori_protocol::NoriEvent::SessionStarted(started),
                        ) => Some(started.clone()),
                        _ => None,
                    };
                    let ended_message = if let Some(CandidateAgent::Activating {
                        widget,
                        activation_error,
                        activation_route_eligible,
                        ..
                    }) = self.candidate_agent.as_mut()
                    {
                        if let nori_protocol::SessionEvent::Acp(
                            nori_protocol::AcpEvent::Response {
                                response: Err(error),
                                ..
                            },
                        ) = &event
                        {
                            *activation_route_eligible |=
                                crate::app::model_routes::eligible_acp_error(error);
                        }
                        match classify_candidate_activation_event(&event, activation_error) {
                            CandidateActivationEvent::Forward => {
                                widget.handle_session_event(generation, event);
                                None
                            }
                            CandidateActivationEvent::CapturedError => None,
                            CandidateActivationEvent::Ended(message) => {
                                widget.handle_session_event(generation, event);
                                Some(message)
                            }
                        }
                    } else {
                        None
                    };

                    if let Some(started) = started {
                        if let Some(handoff) = &self.manual_model_handoff {
                            let still_current = self.chat_widget.session_generation()
                                == handoff.source_generation
                                && !self.chat_widget.is_task_running();
                            let context = handoff.recorder.visible_context().await;
                            if !still_current
                                || context.as_deref().ok() != Some(handoff.context.as_str())
                            {
                                self.discard_candidate();
                                self.manual_model_handoff = None;
                                self.model_route_fallback = None;
                                tui.show_toast(
                                    "Conversation changed; choose the model again".to_owned(),
                                );
                                return Ok(true);
                            }
                        }
                        if let Some((agent, model)) = self.pending_candidate_model.take() {
                            let candidate = match &self.candidate_agent {
                                Some(CandidateAgent::Activating {
                                    agent_name, widget, ..
                                }) if *agent_name == agent => widget.harness_handle(),
                                _ => None,
                            };
                            let config_id = candidate_model_config(&started.config_options, &model);
                            let result = match (candidate.as_ref(), config_id) {
                                (Some(handle), Some(config_id)) => handle
                                    .set_session_config_option(config_id, model.clone())
                                    .await
                                    .map_err(|error| {
                                        (
                                            crate::app::model_routes::eligible_error(&error),
                                            error.to_string(),
                                        )
                                    }),
                                _ => Err((
                                    true,
                                    format!("{agent} no longer advertises model {model}"),
                                )),
                            };
                            match result {
                                Ok(options) => {
                                    if let Some(effort) = self
                                        .manual_model_handoff
                                        .as_ref()
                                        .and_then(|handoff| handoff.effort.clone())
                                    {
                                        let effort_option = options.iter().find(|option| {
                                            crate::nori::agent_config_state::is_effort_option(
                                                option,
                                            )
                                        });
                                        let result = match (candidate.as_ref(), effort_option) {
                                            (Some(handle), Some(option)) => {
                                                handle
                                                    .set_session_config_option(
                                                        option.id.to_string(),
                                                        effort,
                                                    )
                                                    .await
                                            }
                                            (Some(_), None) => Ok(options.clone()),
                                            (None, _) => Err(anyhow::anyhow!(
                                                "Candidate session is unavailable"
                                            )),
                                        };
                                        match result {
                                            Ok(options) => {
                                                if let Some(CandidateAgent::Activating {
                                                    widget,
                                                    ..
                                                }) = self.candidate_agent.as_mut()
                                                {
                                                    widget
                                                        .sync_acp_session_config_snapshot(&options);
                                                }
                                            }
                                            Err(error) => {
                                                self.discard_candidate();
                                                let why = format!(
                                                    "it couldn't keep the effort setting ({error})"
                                                );
                                                if self.advance_model_route(&why).await {
                                                    return Ok(true);
                                                }
                                                self.manual_model_handoff = None;
                                                return Ok(true);
                                            }
                                        }
                                    } else if let Some(CandidateAgent::Activating {
                                        widget, ..
                                    }) = self.candidate_agent.as_mut()
                                    {
                                        widget.sync_acp_session_config_snapshot(&options);
                                    }
                                }
                                Err((eligible, error)) => {
                                    self.discard_candidate();
                                    let model_selection = self.model_route_fallback.is_some();
                                    if eligible && self.advance_model_route(&error).await {
                                        return Ok(true);
                                    }
                                    self.end_model_choice(&error);
                                    if model_selection {
                                        self.manual_model_handoff = None;
                                        return Ok(true);
                                    }
                                    self.chat_widget.add_error_message(format!(
                                        "Couldn't select model {model} for {agent}: {error}"
                                    ));
                                    return Ok(true);
                                }
                            }
                        }
                        if self.manual_model_handoff.is_some() {
                            let handle = match &self.candidate_agent {
                                Some(CandidateAgent::Activating { widget, .. }) => {
                                    widget.harness_handle()
                                }
                                _ => None,
                            };
                            let result = match handle {
                                Some(handle) => handle.record_backend_bound().await,
                                None => {
                                    Err(anyhow::anyhow!("Fallback candidate has no active session"))
                                }
                            };
                            if let Err(error) = result {
                                tracing::warn!(%error, "Failed to record fallback backend binding");
                                self.discard_candidate();
                                self.manual_model_handoff = None;
                                self.model_route_fallback = None;
                                tui.show_toast("Couldn't save the model switch".to_owned());
                                return Ok(true);
                            }
                            if let Some(handle) = self.chat_widget.harness_handle()
                                && let Err(error) = handle.retire_handoff().await
                            {
                                tracing::warn!(%error, "Failed to retire previous backend recorder");
                                if self.manual_model_handoff.is_some() {
                                    if let Err(error) = handle.record_backend_bound().await {
                                        tracing::error!(%error, "Failed to restore the original backend binding");
                                    }
                                    self.discard_candidate();
                                    self.manual_model_handoff = None;
                                    self.model_route_fallback = None;
                                    tui.show_toast("Couldn't finish the model switch".to_owned());
                                    return Ok(true);
                                }
                            }
                        }
                        let Some(CandidateAgent::Activating {
                            agent_name, widget, ..
                        }) = self.candidate_agent.take()
                        else {
                            unreachable!("candidate generation checked above")
                        };
                        self.model_route_fallback = None;
                        let manual_handoff = self.manual_model_handoff.take().is_some();
                        if manual_handoff {
                            self.chat_widget.flush_active_cell();
                        }
                        let mut old_widget = std::mem::replace(&mut self.chat_widget, *widget);
                        let draft = manual_handoff.then(|| old_widget.take_composer_draft());
                        self.session_card_shown = true;
                        self.config.active_agent = agent_name.clone();
                        self.config.agent = agent_name;
                        self.configure_new_chat_widget();
                        if let Some(draft) = draft {
                            self.chat_widget.restore_composer_draft(draft);
                        }
                        self.program_status
                            .event(&nori_protocol::SessionEvent::Nori(
                                nori_protocol::NoriEvent::SessionStarted(started.clone()),
                            ));
                        if let Some(handle) = self.chat_widget.harness_handle()
                            && let Err(error) = self
                                .remote_control
                                .attach_started(handle, self.config.nori_home.clone(), started)
                                .await
                        {
                            tracing::warn!(%error, "failed to attach committed candidate to remote ACP host");
                        }
                        if !manual_handoff {
                            let (initial_prompt, initial_images) = old_widget.take_initial_input();
                            self.chat_widget
                                .submit_launch_input(initial_prompt, initial_images);
                        }
                        self.cancel_primary_agent_preparation();
                        if let Some(agent) = self.prepared_agent.take() {
                            tokio::spawn(agent.shutdown());
                        }
                        self.prepared_agent_initial_context = None;
                        old_widget.shutdown_harness_session();
                        if let Err(error) = ConfigEditsBuilder::new(&self.config.nori_home)
                            .set_agent(&self.config.active_agent)
                            .apply()
                            .await
                        {
                            tracing::error!(%error, "failed to persist activated agent selection");
                        }
                        self.chat_widget.show_agent_in_footer();
                    } else if let Some(message) = ended_message {
                        self.pending_candidate_model = None;
                        let model_selection = self.model_route_fallback.is_some();
                        let candidate = self.candidate_agent.take();
                        if let Some(CandidateAgent::Activating {
                            agent_name,
                            widget,
                            activation_route_eligible,
                            ..
                        }) = candidate
                        {
                            widget.shutdown_harness_session();
                            if activation_route_eligible && self.advance_model_route(&message).await
                            {
                                return Ok(true);
                            }
                            self.end_model_choice(&message);
                            self.chat_widget.set_login_agent_override(Some(agent_name));
                        }
                        if model_selection {
                            self.manual_model_handoff = None;
                            return Ok(true);
                        }
                        self.chat_widget.add_error_message(message);
                    }
                } else {
                    let started = match &event {
                        nori_protocol::SessionEvent::Nori(
                            nori_protocol::NoriEvent::SessionStarted(started),
                        ) if self.chat_widget.session_generation() == generation => {
                            Some(started.clone())
                        }
                        _ => None,
                    };
                    self.chat_widget.handle_session_event(generation, event);
                    // That session has written its card, if it was to; later ones do not repeat it.
                    self.session_card_shown |= started.is_some();
                    if let Some(started) = started
                        && let Some(handle) = self.chat_widget.harness_handle()
                        && let Err(error) = self
                            .remote_control
                            .attach_started(handle, self.config.nori_home.clone(), started)
                            .await
                    {
                        tracing::warn!(%error, "failed to attach session to remote ACP host");
                    }
                }
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
