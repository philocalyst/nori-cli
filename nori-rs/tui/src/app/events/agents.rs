//! Agents, models, routes and logins.

use super::*;

impl App {
    pub(super) async fn handle_agents_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::LoginProvider { agent_name } => {
                self.chat_widget.sign_in(&agent_name);
            }
            AppEvent::SignInMethods { target, methods } => {
                self.chat_widget.on_sign_in_methods(target, methods);
            }
            AppEvent::SignInWith { target, method } => {
                self.chat_widget.run_sign_in(target, method);
            }
            AppEvent::SignInVerified { target, result } => {
                self.chat_widget.on_sign_in_verified(target, result);
            }
            AppEvent::AddAccount { provider, name } => {
                self.chat_widget.add_account(provider, name);
            }
            AppEvent::AccountAdded {
                provider,
                name,
                result,
            } => {
                self.chat_widget.on_account_added(provider, &name, result);
            }
            AppEvent::SelectModelRoutes {
                routes,
                value_name,
                origin,
            } => {
                self.select_model_routes(routes, value_name, origin).await;
            }
            AppEvent::CancelAgentCandidate => {
                self.discard_candidate();
                self.manual_model_handoff = None;
                self.model_route_fallback = None;
                self.chat_widget
                    .add_info_message("Agent switch cancelled.".to_string(), None);
            }
            AppEvent::AgentSpawnFailed { agent_name, error } => {
                tracing::warn!(
                    agent = %agent_name,
                    error = %error,
                    "Agent failed to spawn, opening agent picker"
                );

                self.chat_widget.on_agent_spawn_failed(&agent_name, &error);
            }
            AppEvent::AgentConnecting { display_name } => {
                tracing::info!(
                    display_name = %display_name,
                    "Agent connecting, showing status indicator"
                );
                self.chat_widget.show_connecting_status(&display_name);
            }
            AppEvent::OpenAcpModelPickerUnsupported => {
                self.chat_widget.open_model_unsupported_popup();
            }
            AppEvent::OpenAcpSessionConfigPicker {
                config_options,
                focus_config_id,
            } => {
                self.chat_widget
                    .open_acp_session_config_picker(config_options, focus_config_id);
            }
            AppEvent::OpenAcpSessionConfigValuePicker { option, origin } => {
                self.chat_widget
                    .open_acp_session_config_value_picker(option, origin);
            }
            AppEvent::OpenCustomModelInput {
                config_id,
                option_name,
                origin,
            } => {
                self.chat_widget
                    .open_custom_model_input(config_id, option_name, origin);
            }
            AppEvent::SetAcpSessionConfigOption {
                config_id,
                value,
                option_name,
                is_custom_model,
                origin,
                route_token,
            } => {
                if self.stale_model_route_token(route_token) {
                    return Ok(true);
                }
                if let crate::nori::session_config_picker::ConfigPickerOrigin::Child(key) = origin {
                    let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
                    let target = roster.resolve(crate::subagent_switcher::Target::Subagent(key));
                    let crate::subagent_switcher::Target::Subagent(resolved_key) = target else {
                        return Ok(true);
                    };
                    if let (Some(parent), Some(session_id)) = (
                        self.chat_widget.harness_handle(),
                        self.chat_widget.subagent_session(resolved_key),
                    ) {
                        self.session_view.advance_controls_revision(resolved_key);
                        let tx = self.app_event_tx.clone();
                        tokio::spawn(async move {
                            let result = parent
                                .set_child_config_option(
                                    session_id,
                                    config_id.clone(),
                                    value.clone(),
                                )
                                .await;
                            let route_fallback_eligible = result
                                .as_ref()
                                .err()
                                .is_some_and(crate::app::model_routes::eligible_error);
                            let result = result.map_err(|error| error.to_string());
                            tx.send(AppEvent::SubagentConfigSetResult {
                                key,
                                result,
                                config_id,
                                value,
                                route_fallback_eligible,
                                route_token,
                            });
                        });
                    } else {
                        self.model_route_fallback = None;
                        tui.show_toast(
                            "Model controls are unavailable for this child session.".to_owned(),
                        );
                    }
                } else {
                    self.chat_widget.set_acp_session_config_option(
                        config_id,
                        value,
                        option_name,
                        is_custom_model,
                        origin,
                        route_token,
                    );
                }
            }
            AppEvent::CycleModel(step) => {
                if self.session_view.focused() != crate::subagent_switcher::Target::Main {
                    tui.show_toast(
                        "Model controls are unavailable for this child session.".to_owned(),
                    );
                } else if !self.chat_widget.cycle_model(step)
                    && self
                        .waiting_model_presses
                        .press(step, std::time::Instant::now())
                {
                    self.chat_widget.add_info_message(
                        "The agent hasn't listed its models yet; this applies as soon as it does."
                            .to_owned(),
                        None,
                    );
                }
            }
            AppEvent::CycleEffort(step) => {
                if self.session_view.focused() != crate::subagent_switcher::Target::Main {
                    tui.show_toast(
                        "Effort controls are unavailable for this child session.".to_owned(),
                    );
                } else if !self.chat_widget.cycle_effort(step) {
                    self.chat_widget.add_info_message(
                        "The agent hasn't listed its options yet.".to_owned(),
                        None,
                    );
                }
            }
            AppEvent::CycleModelMode(step) => {
                if self.session_view.focused() != crate::subagent_switcher::Target::Main {
                    tui.show_toast(
                        "Model controls are unavailable for this child session.".to_owned(),
                    );
                } else if !self.chat_widget.cycle_model_mode(step) {
                    self.chat_widget.add_info_message(
                        "The agent hasn't listed its options yet.".to_owned(),
                        None,
                    );
                }
            }
            AppEvent::AcpSessionConfigSetResult {
                success,
                agent,
                config_id,
                value,
                option_name,
                is_custom_model,
                origin,
                config_options,
                error,
                route_fallback_eligible,
                route_token,
            } => {
                // A value chosen in a picker was shown when it was chosen; the answer confirms it
                // or puts the old one back.
                let adopt = origin != crate::nori::session_config_picker::ConfigPickerOrigin::Cycle
                    && self
                        .chat_widget
                        .settle_chosen_option(&config_id, &value, success);
                if self.stale_model_route_token(route_token) {
                    return Ok(true);
                }
                if self.is_model_session_route(route_token, &config_id, &value) {
                    let why = error.as_deref().unwrap_or("the agent refused it");
                    if !success && route_fallback_eligible && self.advance_model_route(why).await {
                        return Ok(true);
                    }
                    self.model_route_fallback = None;
                    self.manual_model_handoff = None;
                }
                let cycled =
                    origin == crate::nori::session_config_picker::ConfigPickerOrigin::Cycle;
                if cycled {
                    if !success {
                        tui.show_toast(format!(
                            "Couldn't update {option_name}: {}",
                            error.as_deref().unwrap_or("Unknown error")
                        ));
                    }
                    self.chat_widget.on_model_cycle_result(if success {
                        Ok(config_options.clone())
                    } else {
                        Err(error.unwrap_or_else(|| "Unknown error".to_string()))
                    });
                    if success
                        && let Err(err) = self
                            .persist_default_model_selection(
                                &agent,
                                &config_id,
                                &value,
                                config_options.as_deref().unwrap_or_default(),
                            )
                            .await
                    {
                        tracing::error!(error = %err, "failed to persist default model selection");
                    }
                } else if success {
                    if let Some(config_options) = &config_options {
                        self.chat_widget
                            .sync_acp_session_config_snapshot(config_options);
                    }
                    if let Err(err) = self
                        .persist_default_model_selection(
                            &agent,
                            &config_id,
                            &value,
                            config_options.as_deref().unwrap_or_default(),
                        )
                        .await
                    {
                        tracing::error!(error = %err, "failed to persist default model selection");
                    }
                } else if is_custom_model
                    && nori_harness::get_agent_config(&agent)
                        .map(|config| config.supports_model_injection())
                        .unwrap_or(false)
                {
                    // The agent rejected this model from its live picker, but we
                    // can still run it: persist it as the default and restart so
                    // it is forced through the agent's spawn-time channel.
                    if self.persist_custom_default_model(&agent, &value).await {
                        self.app_event_tx.send(AppEvent::NewSession);
                    } else {
                        tui.show_toast("Couldn't save the model selection".to_owned());
                    }
                } else {
                    let error_msg = error.unwrap_or_else(|| "Unknown error".to_string());
                    tui.show_toast(format!("Couldn't update {option_name}: {error_msg}"));
                }
            }
            AppEvent::AcpSessionConfigSnapshot {
                generation,
                config_options,
            } => {
                self.chat_widget
                    .handle_acp_session_config_snapshot(generation, &config_options);
            }
            AppEvent::AcpModeConfigSnapshot { generation, mode } => {
                self.chat_widget
                    .apply_acp_mode_config_snapshot(generation, mode);
            }
            AppEvent::LoginStarted { target } => match target {
                Some(key) => {
                    if let Some(session) = self.chat_widget.subagent_session(key) {
                        self.program_status.auth(Some(&session));
                    }
                }
                None => self.program_status.auth(None),
            },
            AppEvent::LoginCancelled => self.program_status.finish_auth(),
            AppEvent::LoginComplete { success } => {
                self.program_status.finish_auth();
                self.chat_widget.handle_login_complete(success);
            }
            AppEvent::ExternalCliLoginOutput { data } => {
                self.chat_widget.handle_external_cli_login_output(data);
            }
            AppEvent::ExternalCliLoginComplete {
                success,
                agent_name,
            } => {
                self.program_status.finish_auth();
                self.chat_widget
                    .handle_external_cli_login_complete(success, agent_name);
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
