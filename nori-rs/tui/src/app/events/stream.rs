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
                        // Its model, effort and binding are put in place on a task; the swap
                        // happens when that is done (`AppEvent::CandidateCommitted`).
                        self.begin_candidate_commit(tui, generation, started);
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
                            if activation_route_eligible && self.advance_model_route(&message) {
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
