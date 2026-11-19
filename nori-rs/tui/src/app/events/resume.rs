//! Resuming a recorded or agent-held session.

use super::*;

impl App {
    pub(super) async fn handle_resume_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::ShowResumeSessionPicker {
                sessions,
                nori_home,
            } => {
                let overlay = crate::resume_picker::helix_overlay::ResumeOverlay::new_local(
                    sessions, nori_home,
                );
                let _ = tui.set_mouse_capture(true);
                self.overlay = Some(Overlay::Resume(Box::new(overlay)));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ShowHelixAcpResumePicker {
                sessions,
                cancel_candidate_on_dismiss,
            } => {
                let overlay = crate::resume_picker::helix_overlay::ResumeOverlay::new_acp(
                    sessions,
                    cancel_candidate_on_dismiss,
                );
                let _ = tui.set_mouse_capture(true);
                self.overlay = Some(Overlay::Resume(Box::new(overlay)));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ResumeSession {
                nori_home,
                project_id,
                session_id,
            } => {
                // Reading and parsing a long transcript must not hold up the UI: it loads on a
                // task and the resume continues when it arrives.
                self.chat_widget
                    .add_info_message("Loading session…".to_string(), None);
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let loader = nori_harness::transcript::TranscriptLoader::new(nori_home.clone());
                    // A delegated child is never a conversation of its own.
                    let allowed = nori_harness::session_index::SessionIndex::open(&nori_home)
                        .refuse_child(&project_id, &session_id)
                        .await;
                    let transcript = match allowed {
                        Ok(()) => loader
                            .load_transcript(&project_id, &session_id)
                            .await
                            .map(Box::new)
                            .map_err(|error| error.to_string()),
                        Err(reason) => Err(reason),
                    };
                    tx.send(AppEvent::ResumeTranscriptLoaded { transcript });
                });
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ResumeTranscriptLoaded { transcript } => match transcript {
                Ok(transcript) => {
                    let transcript = *transcript;
                    let acp_session_id = transcript.latest_acp_session_id();
                    let display_name =
                        crate::nori::agent_picker::get_agent_info(&self.config.active_agent)
                            .map(|info| info.display_name)
                            .unwrap_or_else(|| self.config.active_agent.clone());
                    let loaded_at_startup = matches!(
                        self.pending_session_activation,
                        Some(PendingSessionActivation::ResumeLoading)
                    );
                    if loaded_at_startup {
                        self.pending_session_activation = None;
                    }

                    if self.prepared_agent.is_none()
                        && loaded_at_startup
                        && self.primary_agent_preparation.is_some()
                    {
                        // The agent is still starting; it activates the resume once ready.
                        self.pending_session_activation = Some(PendingSessionActivation::Resume {
                            acp_session_id,
                            title: None,
                            transcript: Some(Box::new(transcript)),
                        });
                        return Ok(true);
                    }
                    if self.prepared_agent.is_none() {
                        self.defer_resume_activation(
                            tui.frame_requester(),
                            acp_session_id,
                            None,
                            Some(transcript),
                        );
                        self.chat_widget.add_info_message(
                            format!("Resuming session with {display_name}..."),
                            None,
                        );
                        tui.frame_requester().schedule_frame();
                        return Ok(true);
                    }

                    // The activation stays pending, transcript and all, unless the agent is
                    // ready.
                    self.pending_session_activation = Some(PendingSessionActivation::Resume {
                        acp_session_id: acp_session_id.clone(),
                        title: None,
                        transcript: Some(Box::new(transcript)),
                    });
                    let agent = match self.take_refreshed_prepared_agent() {
                        Ok(Some(agent)) => agent,
                        Ok(None) => return Ok(true),
                        Err(error) => {
                            self.chat_widget.add_error_message(format!(
                                "Couldn't resume prepared agent: {error}"
                            ));
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
                        ChatWidget::new_resumed_acp(init, acp_session_id, None, transcript);
                    self.configure_new_chat_widget();

                    self.chat_widget
                        .add_info_message(format!("Resuming session with {display_name}..."), None);
                    tui.frame_requester().schedule_frame();
                }
                Err(error) => {
                    self.chat_widget
                        .add_error_message(format!("Failed to load session transcript: {error}"));
                    // A resume chosen at startup falls back to a fresh session.
                    if matches!(
                        self.pending_session_activation,
                        Some(PendingSessionActivation::ResumeLoading)
                    ) {
                        if self.prepared_agent.is_some() {
                            self.pending_session_activation = None;
                            self.app_event_tx.send(AppEvent::NewSession);
                        } else {
                            self.pending_session_activation = Some(PendingSessionActivation::New);
                        }
                    }
                }
            },
            AppEvent::ResumeAcpSession {
                acp_session_id,
                title,
            } => {
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
                    if let Err(error) = nori_harness::runtime::refresh_prepared_agent(
                        &mut agent,
                        crate::chatwidget::agent::agent_prepare_spec(config.clone(), None),
                    ) {
                        tokio::spawn((*agent).shutdown());
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
                    let widget = ChatWidget::new_resumed_acp_candidate(
                        init,
                        Some(acp_session_id),
                        title,
                        None,
                    );
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
                self.cancel_primary_agent_preparation();
                let display_name =
                    crate::nori::agent_picker::get_agent_info(&self.config.active_agent)
                        .map(|info| info.display_name)
                        .unwrap_or_else(|| self.config.active_agent.clone());
                if self.prepared_agent.is_none() {
                    self.defer_resume_activation(
                        tui.frame_requester(),
                        Some(acp_session_id.clone()),
                        title.clone(),
                        None,
                    );
                    self.chat_widget.add_info_message(
                        reattach_info_message(
                            &acp_session_id,
                            title.as_deref(),
                            self.cloud_mode,
                            &display_name,
                        ),
                        None,
                    );
                    tui.frame_requester().schedule_frame();
                    return Ok(true);
                }
                self.pending_session_activation = Some(PendingSessionActivation::Resume {
                    acp_session_id: Some(acp_session_id.clone()),
                    title: title.clone(),
                    transcript: None,
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
                self.pending_session_activation = None;
                let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
                self.deferred_spawn_pending = false;
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
                self.chat_widget = ChatWidget::new_resumed_acp(
                    init,
                    Some(acp_session_id.clone()),
                    title.clone(),
                    None,
                );
                self.configure_new_chat_widget();

                self.chat_widget.add_info_message(
                    reattach_info_message(
                        &acp_session_id,
                        title.as_deref(),
                        self.cloud_mode,
                        &display_name,
                    ),
                    None,
                );
                tui.frame_requester().schedule_frame();
            }
            #[cfg(unix)]
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
