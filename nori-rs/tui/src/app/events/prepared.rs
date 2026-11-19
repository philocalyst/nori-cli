//! An agent that is ready: preparing it, and activating a prepared resume.

use super::*;

impl App {
    pub(super) async fn handle_prepared_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::AgentPrepared {
                generation,
                agent,
                intent,
            } => {
                if let crate::app_event::AgentPrepareIntent::Candidate {
                    agent_name,
                    display_name,
                } = intent
                {
                    let is_current = matches!(
                        &self.candidate_agent,
                        Some(CandidateAgent::Preparing {
                            generation: current,
                            agent_name: current_agent,
                            ..
                        }) if *current == generation && *current_agent == agent_name
                    );
                    if !is_current {
                        if let Ok(agent) = agent {
                            tokio::spawn(agent.shutdown());
                        }
                        return Ok(true);
                    }

                    match agent {
                        Ok(agent) => {
                            let Some(CandidateAgent::Preparing { agent_name, .. }) =
                                self.candidate_agent.take()
                            else {
                                unreachable!("candidate generation checked above")
                            };
                            self.candidate_agent = Some(CandidateAgent::Prepared {
                                agent_name,
                                display_name,
                                agent: Box::new(agent),
                            });
                            if self.pending_candidate_model.is_some() {
                                self.app_event_tx.send(AppEvent::NewSession);
                            } else {
                                self.follow_up_prepared_candidate();
                            }
                        }
                        Err(error) => {
                            self.candidate_agent = None;
                            self.pending_candidate_model = None;
                            let model_selection = self.model_route_fallback.is_some();
                            if crate::app::model_routes::eligible_error(&error)
                                && self.advance_model_route().await
                            {
                                return Ok(true);
                            }
                            self.model_route_fallback = None;
                            if model_selection {
                                self.manual_model_handoff = None;
                                tui.show_toast(format!("Couldn't select model: {error}"));
                                return Ok(true);
                            }
                            self.chat_widget.set_login_agent_override(Some(agent_name));
                            self.chat_widget.add_error_message(format!(
                                "Couldn't prepare {display_name}: {error}"
                            ));
                        }
                    }
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }

                let is_current = matches!(
                    &self.primary_agent_preparation,
                    Some(preparation) if preparation.generation == generation
                );
                if !is_current {
                    if let Ok(agent) = agent {
                        tokio::spawn(agent.shutdown());
                    }
                    return Ok(true);
                }
                let Some(primary_preparation) = self.primary_agent_preparation.as_ref() else {
                    return Ok(true);
                };
                let prepare_intent = primary_preparation.intent.clone();
                let prepared_initial_context = primary_preparation.initial_context.clone();
                self.primary_agent_preparation = None;

                match agent {
                    Ok(agent) => {
                        let automatic_resume = super::session_setup::automatic_resume_event(
                            agent.automatic_session_id(),
                        );
                        // Seed the deferred widget with the prepared agent
                        // capabilities so capability-gated behavior (e.g. the
                        // detach wording on quit) is right before any session
                        // exists.
                        self.chat_widget.handle_client_event(
                            crate::presentation::ClientEvent::SessionCapabilitiesChanged(
                                crate::presentation::SessionCapabilitiesView {
                                    agent: crate::presentation::AgentCapabilitiesView {
                                        http_mcp: agent.capabilities().mcp_capabilities.http,
                                        load_session: agent.capabilities().load_session,
                                        session_list: agent
                                            .capabilities()
                                            .session_capabilities
                                            .list
                                            .is_some(),
                                        session_resume: agent
                                            .capabilities()
                                            .session_capabilities
                                            .resume
                                            .is_some(),
                                        session_close: agent
                                            .capabilities()
                                            .session_capabilities
                                            .close
                                            .is_some(),
                                        session_fork: agent
                                            .capabilities()
                                            .session_capabilities
                                            .fork
                                            .is_some(),
                                    },
                                    ..Default::default()
                                },
                            ),
                        );
                        let can_resume_catalog = agent.capabilities().load_session
                            || agent.capabilities().session_capabilities.resume.is_some();
                        let sessions = match agent.catalog() {
                            nori_harness::runtime::SessionCatalog::Unsupported => None,
                            nori_harness::runtime::SessionCatalog::Listed(sessions) => {
                                Some(sessions.clone())
                            }
                        };
                        self.prepared_agent = Some(agent);
                        self.prepared_agent_initial_context = prepared_initial_context;
                        if let Some(event) = automatic_resume {
                            self.pending_session_activation = None;
                            self.app_event_tx.send(event);
                        } else {
                            if let Some(activation) = self.pending_session_activation.take() {
                                match activation {
                                    PendingSessionActivation::New => {
                                        self.app_event_tx.send(AppEvent::NewSession)
                                    }
                                    // The prepared agent waits for the transcript, which
                                    // activates it when it arrives.
                                    PendingSessionActivation::ResumeLoading => {
                                        self.pending_session_activation =
                                            Some(PendingSessionActivation::ResumeLoading);
                                    }
                                    PendingSessionActivation::Resume {
                                        acp_session_id,
                                        title,
                                        transcript,
                                    } => self.app_event_tx.send(AppEvent::ActivatePreparedResume {
                                        acp_session_id,
                                        title,
                                        transcript: transcript.map(|transcript| *transcript),
                                    }),
                                }
                                tui.frame_requester().schedule_frame();
                                return Ok(true);
                            }
                            match prepare_intent {
                                crate::app_event::AgentPrepareIntent::Idle => {}
                                crate::app_event::AgentPrepareIntent::Onboarding => {
                                    if let Some(event) = sessions
                                        .and_then(super::session_setup::onboarding_resume_event)
                                    {
                                        self.app_event_tx.send(event);
                                    } else {
                                        self.app_event_tx.send(AppEvent::NewSession);
                                    }
                                }
                                crate::app_event::AgentPrepareIntent::Picker {
                                    fallback_to_spawn,
                                } => match sessions {
                                    Some(sessions) => self
                                        .chat_widget
                                        .show_acp_resume_session_picker(sessions, false),
                                    None if fallback_to_spawn => {
                                        self.app_event_tx.send(AppEvent::NewSession)
                                    }
                                    None => self
                                        .chat_widget
                                        .show_acp_resume_session_picker(Vec::new(), false),
                                },
                                crate::app_event::AgentPrepareIntent::ResumePicker => {
                                    if can_resume_catalog && let Some(sessions) = sessions {
                                        self.chat_widget
                                            .show_acp_resume_session_picker(sessions, false);
                                    } else {
                                        self.chat_widget.open_local_resume_session_picker();
                                    }
                                }
                                crate::app_event::AgentPrepareIntent::Candidate { .. } => {
                                    unreachable!("candidate handled above")
                                }
                            }
                        }
                    }
                    Err(error) => {
                        self.prepared_agent_initial_context = None;
                        self.deferred_spawn_pending = false;
                        self.chat_widget
                            .add_error_message(format!("Couldn't prepare agent: {error}"));
                        self.chat_widget.add_info_message(
                            "No session is active - /resume retries the picker, /new starts a \
                             fresh session."
                                .to_string(),
                            None,
                        );
                        if !self.cloud_mode && self.candidate_agent.is_none() {
                            self.chat_widget
                                .open_agent_recovery_popup(&format!("{error:#}"));
                        }
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ActivatePreparedResume {
                acp_session_id,
                title,
                transcript,
            } => {
                if self.prepared_agent.is_none() {
                    self.pending_session_activation = Some(PendingSessionActivation::Resume {
                        acp_session_id,
                        title,
                        transcript: transcript.map(Box::new),
                    });
                    self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                    return Ok(true);
                }
                // The activation stays pending, transcript and all, unless the agent is ready.
                self.pending_session_activation = Some(PendingSessionActivation::Resume {
                    acp_session_id: acp_session_id.clone(),
                    title: title.clone(),
                    transcript: transcript.map(Box::new),
                });
                let agent = match self.take_refreshed_prepared_agent() {
                    Ok(Some(agent)) => agent,
                    Ok(None) => return Ok(true),
                    Err(error) => {
                        self.chat_widget
                            .add_error_message(format!("Couldn't resume prepared agent: {error}"));
                        return Ok(true);
                    }
                };
                let transcript =
                    take_pending_resume_transcript(&mut self.pending_session_activation);
                let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
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
                self.chat_widget =
                    ChatWidget::new_resumed_acp(init, acp_session_id, title, transcript);
                self.configure_new_chat_widget();
                tui.frame_requester().schedule_frame();
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
