//! Skillsets, scripts, MCP servers and loops.

use super::*;

impl App {
    pub(super) async fn handle_extensions_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::LoopIteration {
                prompt,
                remaining,
                total,
            } => {
                let iteration = total - remaining;
                tracing::info!("Loop iteration {iteration}/{total} (remaining: {remaining})");

                self.shutdown_current_conversation();

                let init = self.chat_widget_init(
                    tui.frame_requester(),
                    Some(prompt),
                    Vec::new(),
                    None,
                    true,
                    None,
                );
                self.chat_widget = ChatWidget::new(init);
                self.configure_new_chat_widget();
                self.chat_widget.set_loop_state(remaining, total);
                self.pending_session_activation = Some(PendingSessionActivation::New);
                self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);

                self.chat_widget
                    .add_info_message(format!("Loop iteration {iteration} of {total}"), None);
            }
            AppEvent::SkillsetListResult {
                names,
                error,
                install_dir,
            } => {
                self.chat_widget
                    .on_skillset_list_result(names, error, install_dir);
            }
            AppEvent::InstallSkillset { name } => {
                self.chat_widget.on_install_skillset_request(&name);
            }
            AppEvent::SwitchSkillset { name, install_dir } => {
                self.chat_widget
                    .on_switch_skillset_request(&name, &install_dir);
            }
            AppEvent::SkillsetInstallResult {
                name,
                success,
                message,
            } => {
                self.chat_widget
                    .on_skillset_install_result(&name, success, &message);
                self.on_skillset_applied(success);
            }
            AppEvent::SkillsetSwitchResult {
                name,
                success,
                message,
            } => {
                self.chat_widget
                    .on_skillset_switch_result(&name, success, &message);
                self.on_skillset_applied(success);
            }
            AppEvent::SkillsetPickerDismissed => {
                // The skillset picker was dismissed without selection. If the
                // agent spawn was deferred, spawn it now without a skillset
                // (behaves as if skillset_per_session is disabled) and activate
                // the session, matching plain local startup.
                if self.take_deferred_spawn() {
                    self.pending_session_activation = Some(PendingSessionActivation::New);
                    self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
                }
            }
            AppEvent::ExecuteScript { prompt, args } => {
                let tx = self.app_event_tx.clone();
                let timeout = self.config.script_timeout.as_duration();
                let name = prompt.name.clone();
                self.chat_widget
                    .add_info_message(format!("Running script '{name}'..."), None);
                tokio::spawn(async move {
                    let result =
                        nori_harness::custom_prompts::execute_script(&prompt, &args, timeout).await;
                    tx.send(AppEvent::ScriptExecutionComplete {
                        name: prompt.name.clone(),
                        result,
                    });
                });
            }
            AppEvent::ScriptExecutionComplete { name, result } => match result {
                Ok(stdout) => {
                    if stdout.trim().is_empty() {
                        self.chat_widget.add_info_message(
                            format!("Script '{name}' completed with no output."),
                            None,
                        );
                    } else {
                        let message = format!("Output from script '{name}':\n{stdout}");
                        self.chat_widget.queue_text_as_user_message(message);
                    }
                }
                Err(err) => {
                    self.chat_widget
                        .add_error_message(format!("Script '{name}' failed: {err}"));
                    let error_context =
                        format!("Script '{name}' failed with the following error:\n{err}");
                    self.chat_widget.queue_text_as_user_message(error_context);
                }
            },
            AppEvent::SaveMcpServers(servers) => {
                self.persist_mcp_servers(servers);
            }
            AppEvent::McpOAuthLogin {
                server_name,
                server_url,
                http_headers,
                env_http_headers,
                client_id,
                client_secret_env_var,
            } => {
                self.program_status.auth(None);
                self.perform_mcp_oauth_login(
                    tui,
                    server_name,
                    server_url,
                    http_headers,
                    env_http_headers,
                    client_id,
                    client_secret_env_var,
                )
                .await;
            }
            AppEvent::McpOAuthLoginCancel { server_name } => {
                self.program_status.finish_auth();
                tracing::info!("MCP OAuth login cancelled for {server_name}");
                self.cancel_mcp_oauth_login();
                self.chat_widget.add_info_message(
                    format!("OAuth cancelled for `{server_name}`. Press `l` in /mcp to try again."),
                    None,
                );
            }
            AppEvent::McpOAuthLoginComplete {
                server_name,
                success,
                error,
            } => {
                self.program_status.finish_auth();
                if success {
                    self.chat_widget.add_info_message(
                        format!(
                            "Successfully authenticated with `{server_name}`. Restart to apply."
                        ),
                        None,
                    );
                } else {
                    let msg = match error {
                        Some(err) => {
                            format!("OAuth login for `{server_name}` failed: {err}")
                        }
                        None => format!("OAuth login for `{server_name}` failed."),
                    };
                    self.chat_widget.add_error_message(msg);
                }
                self.notify_choice(&(server_name, success));
            }
            AppEvent::ComputeMcpAuthStatuses => {
                let servers = self.chat_widget.config_ref().mcp_servers.clone();
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let mut statuses = std::collections::HashMap::new();
                    for (name, config) in servers {
                        let status = match config.transport {
                            nori_config::McpServerTransportConfig::Stdio {
                                ..
                            } => codex_rmcp_client::McpAuthStatus::Unsupported,
                            nori_config::McpServerTransportConfig::StreamableHttp {
                                url,
                                bearer_token_env_var,
                                http_headers,
                                env_http_headers,
                                ..
                            } => codex_rmcp_client::determine_streamable_http_auth_status(
                                &name,
                                &url,
                                bearer_token_env_var.as_deref(),
                                http_headers,
                                env_http_headers,
                                codex_rmcp_client::OAuthCredentialsStoreMode::Auto,
                            )
                            .await
                            .unwrap_or_else(|err| {
                                tracing::warn!(
                                    "failed to determine auth status for MCP server `{name}`: {err:?}"
                                );
                                codex_rmcp_client::McpAuthStatus::Unsupported
                            }),
                        };
                        statuses.insert(name, status);
                    }
                    tx.send(AppEvent::McpAuthStatusesReady(statuses));
                });
            }
            AppEvent::McpAuthStatusesReady(statuses) => {
                self.notify_choice(&statuses);
                tui.frame_requester().schedule_frame();
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
