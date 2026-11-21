use super::*;
use crate::nori::config_picker::SettingsItem;
use nori_config::NoriConfigEdits as ConfigEditsBuilder;
use std::path::Path;

/// Whether `config_id` names the Model-category option among `config_options`.
fn is_model_option(
    config_id: &str,
    config_options: &[nori_protocol::acp::v1::SessionConfigOption],
) -> bool {
    config_options.iter().any(|option| {
        option.id.to_string() == config_id
            && option.category == Some(nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
    })
}

/// Persist a session config option selection as the agent's default model.
///
/// Returns `Ok(true)` only when `config_id` names the agent's Model-category
/// option and the selection was written to `[default_models]` in config.toml.
/// Non-model options (mode, thought level, ...) are not persisted. The write (a file
/// replacement and a disk flush) runs on a blocking thread.
pub(super) async fn persist_default_model_selection(
    nori_home: &Path,
    agent: &str,
    config_id: &str,
    value: &str,
    config_options: &[nori_protocol::acp::v1::SessionConfigOption],
) -> anyhow::Result<bool> {
    if !is_model_option(config_id, config_options) {
        return Ok(false);
    }
    ConfigEditsBuilder::new(nori_home)
        .set_default_model(agent, value)
        .apply()
        .await?;
    Ok(true)
}

impl App {
    fn sync_runtime_config(&mut self) {
        self.chat_widget.set_config(self.config.clone());
    }

    /// Remember the agent's chosen model as its default. The choice is in effect at once; the
    /// write to config.toml happens in the background, so the event loop never waits for the
    /// disk. A failed write is logged.
    pub(super) fn persist_default_model_selection(
        &mut self,
        agent: &str,
        config_id: &str,
        value: &str,
        config_options: &[nori_protocol::acp::v1::SessionConfigOption],
    ) {
        if !is_model_option(config_id, config_options) {
            return;
        }
        self.config
            .default_models
            .insert(agent.to_string(), value.to_string());
        self.sync_runtime_config();
        let nori_home = self.config.nori_home.clone();
        let (agent, config_id, value) = (agent.to_owned(), config_id.to_owned(), value.to_owned());
        let config_options = config_options.to_vec();
        tokio::spawn(async move {
            if let Err(error) = persist_default_model_selection(
                &nori_home,
                &agent,
                &config_id,
                &value,
                &config_options,
            )
            .await
            {
                tracing::error!(%error, "failed to persist default model selection");
            }
        });
    }

    /// Make a custom model the agent's default and start a new session with it.
    ///
    /// Used when the agent rejected the model from its live picker: there is no
    /// config-options snapshot to categorize, but the model came from the
    /// free-text custom-model input, so it is a model by construction. The new
    /// session forces it through the agent's spawn-time injection channel, so it
    /// starts once the default is on disk; the event loop does not wait for that.
    pub(super) fn persist_custom_default_model_and_restart(&mut self, agent: &str, value: &str) {
        self.config
            .default_models
            .insert(agent.to_string(), value.to_string());
        self.sync_runtime_config();
        let edits = ConfigEditsBuilder::new(&self.config.nori_home).set_default_model(agent, value);
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            match edits.apply().await {
                Ok(()) => tx.send(AppEvent::NewSession),
                Err(error) => {
                    tracing::error!(%error, "failed to persist custom default model");
                    tx.send(AppEvent::InsertHistoryCell(Box::new(
                        crate::history_cell::new_error_event(format!(
                            "Couldn't save the model selection ({error})."
                        )),
                    )));
                }
            }
        });
    }

    /// Save `edits` to config.toml in the background, on the config writer. The setting is in
    /// effect at once and the event loop never waits for the disk; a write that fails says so
    /// in the transcript, since the change then lasts for this session only.
    pub(super) fn save_config(&self, what: &str, edits: ConfigEditsBuilder) {
        let tx = self.app_event_tx.clone();
        let what = what.to_owned();
        tokio::spawn(async move {
            if let Err(error) = edits.apply().await {
                tracing::error!(%error, "failed to save {what}");
                tx.send(AppEvent::InsertHistoryCell(Box::new(
                    crate::history_cell::new_error_event(format!(
                        "Couldn't save {what} to config.toml ({error}); it applies to this session only."
                    )),
                )));
            }
        });
    }

    /// Persist a TUI config setting to config.toml and apply it immediately.
    pub(super) fn persist_config_setting(&mut self, setting_name: &str, enabled: bool) {
        match setting_name {
            "vertical_footer" => {}
            _ => {
                tracing::warn!("Unknown config setting: {setting_name}");
                return;
            }
        }

        // Persist to config.toml
        self.save_config(
            &format!("{setting_name} setting"),
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", setting_name], enabled),
        );

        self.config.vertical_footer = enabled;
        self.sync_runtime_config();
        self.vertical_footer = enabled;
        self.chat_widget.set_vertical_footer(enabled);

        let status = if enabled { "enabled" } else { "disabled" };
        self.chat_widget
            .add_info_message(format!("{setting_name} {status}"), None);
        self.chat_widget
            .reopen_settings_focused(SettingsItem::VerticalFooter);
    }

    pub(super) fn persist_notify_after_idle_setting(
        &mut self,
        value: nori_config::NotifyAfterIdle,
    ) {
        let toml_str = value.toml_value();

        self.save_config(
            "notify_after_idle setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "notify_after_idle"], toml_str),
        );

        self.config.notify_after_idle = value;
        self.sync_runtime_config();
        self.chat_widget.add_info_message(
            format!("Notify after idle set to {}.", value.display_name()),
            None,
        );
        self.chat_widget
            .reopen_settings_focused(SettingsItem::NotifyAfterIdle);
    }

    /// Persist the chosen `/browser` profile as the new default.
    ///
    /// The `/browser` picker resolves the current default fresh from disk on
    /// every launch, so this only records the choice; the caller launches with
    /// it separately.
    #[cfg(unix)]
    pub(super) fn persist_browser_profile_setting(
        &mut self,
        value: nori_config::BrowserProfileMode,
    ) {
        self.save_config(
            "browser_profile setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["browser_profile"], value.toml_value()),
        );

        self.config.browser_profile = value;
        self.sync_runtime_config();
    }

    pub(super) fn persist_script_timeout_setting(&mut self, value: nori_config::ScriptTimeout) {
        let toml_str = value.toml_value();

        self.save_config(
            "script_timeout setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "script_timeout"], toml_str),
        );

        self.config.script_timeout = value.clone();
        self.sync_runtime_config();
        self.chat_widget.add_info_message(
            format!("Script timeout set to {}.", value.display_name()),
            None,
        );
        self.chat_widget
            .reopen_settings_focused(SettingsItem::ScriptTimeout);
    }

    /// Store the loop count as an ephemeral per-session override (not persisted
    /// to the TOML config). The user can still edit the home TOML directly for
    /// a persistent change.
    pub(super) fn set_session_loop_count(&mut self, value: Option<i32>) {
        self.loop_count_override = Some(value);
        self.chat_widget.set_loop_count_override(Some(value));

        let display = match value {
            Some(n) => format!("{n}"),
            None => "Disabled".to_string(),
        };
        self.chat_widget
            .add_info_message(format!("Loop count set to {display} (this session)."), None);
        self.chat_widget
            .reopen_settings_focused(SettingsItem::LoopCount);
    }

    pub(super) fn persist_vim_mode_setting(
        &mut self,
        value: nori_config::VimEnterBehavior,
        from_settings: bool,
    ) {
        self.save_config(
            "vim_mode setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "vim_mode"], value.toml_value()),
        );

        // Update in-memory state and propagate to the chat widget
        self.config.vim_mode = value;
        self.sync_runtime_config();
        self.vim_mode = value;
        self.chat_widget.set_vim_mode(value);

        let display = value.display_name();
        self.chat_widget
            .add_info_message(format!("Vim mode: {display}."), None);

        // Only return to the settings panel when the change came from it; the
        // standalone `/vim` command should just close.
        if from_settings {
            self.chat_widget
                .reopen_settings_focused(SettingsItem::VimMode);
        }
    }

    pub(super) fn persist_auto_worktree_setting(&mut self, value: nori_config::AutoWorktree) {
        let toml_str = value.toml_value();

        self.save_config(
            "auto_worktree setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "auto_worktree"], toml_str),
        );

        self.config.auto_worktree = value;
        self.sync_runtime_config();
        self.chat_widget.add_info_message(
            format!(
                "Auto worktree set to {}. Changes will take effect on next session.",
                value.display_name()
            ),
            None,
        );
        self.chat_widget
            .reopen_settings_focused(SettingsItem::AutoWorktree);
    }

    pub(super) fn persist_pinned_plan_drawer_setting(&mut self, enabled: bool) {
        self.save_config(
            "pinned_plan_drawer setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "pinned_plan_drawer"], enabled),
        );
        self.config.pinned_plan_drawer = enabled;
        self.sync_runtime_config();
        let mode = if enabled {
            crate::chatwidget::PlanDrawerMode::Expanded
        } else {
            crate::chatwidget::PlanDrawerMode::Off
        };
        self.plan_drawer_mode = mode;
        self.chat_widget.set_plan_drawer_mode(mode);
        let status = if enabled { "enabled" } else { "disabled" };
        self.chat_widget
            .add_info_message(format!("Pinned plan drawer {status}."), None);
        self.chat_widget
            .reopen_settings_focused(SettingsItem::PinnedPlanDrawer);
    }

    pub(super) fn persist_custom_working_messages_setting(&mut self, enabled: bool) {
        self.save_config(
            "custom_working_messages setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "custom_working_messages"], enabled),
        );
        self.config.custom_working_messages = enabled;
        self.sync_runtime_config();
        self.chat_widget.set_custom_working_messages(enabled);
        let status = if enabled { "enabled" } else { "disabled" };
        self.chat_widget
            .add_info_message(format!("Custom working messages {status}."), None);
        self.chat_widget
            .reopen_settings_focused(SettingsItem::CustomWorkingMessages);
    }

    pub(super) fn persist_skillset_per_session_setting(&mut self, enabled: bool) {
        self.save_config(
            "skillset_per_session setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "skillset_per_session"], enabled),
        );
        self.config.skillset_per_session = enabled;
        self.sync_runtime_config();
        let status = if enabled { "enabled" } else { "disabled" };
        self.chat_widget.add_info_message(
            format!("Per-session skillsets {status}. Changes will take effect on next session."),
            None,
        );
        self.chat_widget
            .reopen_settings_focused(SettingsItem::PerSessionSkillsets);
    }

    pub(super) fn persist_footer_segment_setting(
        &mut self,
        segment: nori_config::FooterSegment,
        enabled: bool,
    ) {
        self.save_config(
            "footer segment setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "footer_segments", segment.toml_key()], enabled),
        );

        // Update the local config and apply to the widget
        self.footer_segment_config.set_enabled(segment, enabled);
        self.config.footer_segment_config = self.footer_segment_config.clone();
        self.sync_runtime_config();
        self.chat_widget
            .set_footer_segment_enabled(segment, enabled);

        let status = if enabled { "enabled" } else { "disabled" };
        self.chat_widget
            .add_info_message(format!("{} {status}.", segment.display_name()), None);

        // Refresh the picker to show updated state without stacking a new view.
        self.chat_widget
            .replace_footer_segments_picker(&self.footer_segment_config);
    }

    pub(super) fn persist_file_manager_setting(&mut self, value: nori_config::FileManager) {
        self.save_config(
            "file_manager setting",
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "file_manager"], value.command_name()),
        );

        self.config.file_manager = Some(value);
        self.sync_runtime_config();
        self.chat_widget.add_info_message(
            format!("File manager set to {}.", value.display_name()),
            None,
        );
        self.chat_widget
            .reopen_settings_focused(SettingsItem::FileManager);
    }

    pub(super) fn persist_notification_setting(&mut self, setting_name: &str, enabled: bool) {
        if !matches!(setting_name, "terminal_notifications" | "os_notifications") {
            tracing::warn!("Unknown notification setting: {setting_name}");
            return;
        }
        let enum_value = if enabled { "enabled" } else { "disabled" };

        // Persist to config.toml as a string enum value
        self.save_config(
            &format!("{setting_name} setting"),
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", setting_name], enum_value),
        );

        match setting_name {
            "terminal_notifications" => {
                self.config.terminal_notifications = if enabled {
                    nori_config::TerminalNotifications::Enabled
                } else {
                    nori_config::TerminalNotifications::Disabled
                };
            }
            "os_notifications" => {
                self.config.os_notifications = if enabled {
                    nori_config::OsNotifications::Enabled
                } else {
                    nori_config::OsNotifications::Disabled
                };
            }
            _ => unreachable!("notification setting was validated before persistence"),
        }
        self.sync_runtime_config();
        let status = if enabled { "enabled" } else { "disabled" };
        self.chat_widget
            .add_info_message(format!("{setting_name} {status}"), None);
        let focus = match setting_name {
            "terminal_notifications" => SettingsItem::TerminalNotifications,
            "os_notifications" => SettingsItem::OsNotifications,
            _ => unreachable!("notification setting was validated before persistence"),
        };
        self.chat_widget.reopen_settings_focused(focus);
    }

    pub(super) fn persist_hotkey_setting(
        &mut self,
        action: nori_config::HotkeyAction,
        binding: nori_config::HotkeyBinding,
    ) {
        let toml_key = action.toml_key();
        let toml_val = binding.toml_value();

        self.save_config(
            &format!("the hotkey for {}", action.display_name()),
            ConfigEditsBuilder::new(&self.config.nori_home)
                .set_path(&["tui", "hotkeys", toml_key], toml_val),
        );

        self.hotkey_config.set_binding(action, binding.clone());
        self.config.hotkeys = self.hotkey_config.clone();
        self.sync_runtime_config();
        self.chat_widget
            .set_hotkey_config(self.hotkey_config.clone());
        self.chat_widget.add_info_message(
            format!(
                "{} bound to {}",
                action.display_name(),
                binding.display_name()
            ),
            None,
        );
    }

    pub(super) fn persist_mcp_servers(
        &mut self,
        servers: std::collections::BTreeMap<String, nori_config::McpServerConfig>,
    ) {
        self.save_config(
            "MCP servers",
            ConfigEditsBuilder::new(&self.config.nori_home).replace_mcp_servers(&servers),
        );

        // Sync in-memory state so that ComputeMcpAuthStatuses (which reads
        // chat_widget.config_ref().mcp_servers) sees the newly added servers.
        self.config.mcp_servers = servers.into_iter().collect();
        self.sync_runtime_config();

        self.chat_widget.add_info_message(
            "MCP servers saved. Start a new session to use them.".to_string(),
            None,
        );
    }

    /// Start an async MCP OAuth login flow (no TUI suspension).
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn perform_mcp_oauth_login(
        &mut self,
        _tui: &mut crate::tui::Tui,
        server_name: String,
        server_url: String,
        http_headers: Option<std::collections::HashMap<String, String>>,
        env_http_headers: Option<std::collections::HashMap<String, String>>,
        client_id: Option<String>,
        client_secret_env_var: Option<String>,
    ) {
        // Resolve client_secret from env var if provided.
        let client_secret = client_secret_env_var.as_deref().and_then(|var| {
            match std::env::var(var) {
                Ok(val) => Some(val),
                Err(_) => {
                    tracing::warn!(
                        "MCP OAuth: client_secret_env_var `{var}` is not set for `{server_name}`"
                    );
                    None
                }
            }
        });

        let app_event_tx = self.app_event_tx.clone();
        let name_for_task = server_name.clone();

        match codex_rmcp_client::start_oauth_login(
            server_name.clone(),
            server_url,
            codex_rmcp_client::OAuthCredentialsStoreMode::Auto,
            http_headers,
            env_http_headers,
            vec![],
            client_id,
            client_secret,
        )
        .await
        {
            Ok(handle) => {
                self.chat_widget.add_info_message(
                    mcp_oauth_login_started_message(&server_name, &handle.authorization_url),
                    Some("Press Esc in the MCP picker to cancel".to_string()),
                );

                // Store the cancel sender and spawn the task watcher.
                self.mcp_oauth_cancel_tx = handle.cancel_tx;
                let task = handle.task;

                let tx = app_event_tx;
                tokio::spawn(async move {
                    let result = task.await;
                    let (success, error) = match result {
                        Ok(Ok(())) => (true, None),
                        Ok(Err(e)) => (false, Some(format_mcp_oauth_error(&e))),
                        Err(e) => (false, Some(format!("OAuth task panicked: {e}"))),
                    };
                    tx.send(crate::app_event::AppEvent::McpOAuthLoginComplete {
                        server_name: name_for_task,
                        success,
                        error,
                    });
                });
            }
            Err(err) => {
                // Send completion event — the handler will display the error
                // and forward to the picker.
                app_event_tx.send(crate::app_event::AppEvent::McpOAuthLoginComplete {
                    server_name: name_for_task,
                    success: false,
                    error: Some(err.to_string()),
                });
            }
        }
    }

    /// Cancel the in-progress MCP OAuth login flow.
    pub(super) fn cancel_mcp_oauth_login(&mut self) {
        if let Some(tx) = self.mcp_oauth_cancel_tx.take() {
            let _ = tx.send(());
        }
    }
}

fn mcp_oauth_login_started_message(server_name: &str, authorization_url: &str) -> String {
    format!(
        "Opening browser to authenticate `{server_name}`...\n\nIf the browser doesn't open automatically, visit:\n{authorization_url}\n\nWaiting for authentication to complete..."
    )
}

fn format_mcp_oauth_error(error: &anyhow::Error) -> String {
    format!("{error:#}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn mcp_oauth_login_started_message_includes_manual_url() {
        let message = mcp_oauth_login_started_message("linear", "https://linear.example.com/oauth");

        assert!(message.contains("Opening browser to authenticate `linear`"));
        assert!(message.contains("If the browser doesn't open automatically, visit:"));
        assert!(message.contains("https://linear.example.com/oauth"));
        assert!(message.contains("Waiting for authentication to complete"));
    }

    fn session_config_options_with_model() -> Vec<nori_protocol::acp::v1::SessionConfigOption> {
        vec![
            nori_protocol::acp::v1::SessionConfigOption::select(
                "model",
                "Model",
                "sonnet",
                vec![
                    nori_protocol::acp::v1::SessionConfigSelectOption::new("sonnet", "Sonnet"),
                    nori_protocol::acp::v1::SessionConfigSelectOption::new("opus", "Opus"),
                ],
            )
            .category(nori_protocol::acp::v1::SessionConfigOptionCategory::Model),
            nori_protocol::acp::v1::SessionConfigOption::select(
                "permission-mode",
                "Mode",
                "default",
                vec![
                    nori_protocol::acp::v1::SessionConfigSelectOption::new("default", "Default"),
                    nori_protocol::acp::v1::SessionConfigSelectOption::new(
                        "acceptEdits",
                        "Accept Edits",
                    ),
                ],
            )
            .category(nori_protocol::acp::v1::SessionConfigOptionCategory::Mode),
        ]
    }

    #[tokio::test]
    async fn model_selection_persists_default_model_for_agent() {
        let temp = TempDir::new().expect("temp home");

        let persisted = persist_default_model_selection(
            temp.path(),
            "claude-code",
            "model",
            "opus",
            &session_config_options_with_model(),
        )
        .await
        .expect("persist model selection");

        assert!(persisted, "model selection should be persisted");
        let content = std::fs::read_to_string(temp.path().join("config.toml"))
            .expect("read persisted config");
        let parsed: toml::Value = toml::from_str(&content).expect("config toml");
        assert_eq!(
            parsed
                .get("default_models")
                .and_then(|section| section.get("claude-code"))
                .and_then(toml::Value::as_str),
            Some("opus")
        );
    }

    #[tokio::test]
    async fn non_model_selection_is_not_persisted() {
        let temp = TempDir::new().expect("temp home");

        let persisted = persist_default_model_selection(
            temp.path(),
            "claude-code",
            "permission-mode",
            "acceptEdits",
            &session_config_options_with_model(),
        )
        .await
        .expect("handle mode selection");

        assert!(!persisted, "mode selection should not be persisted");
        assert!(
            !temp.path().join("config.toml").exists(),
            "no config file should be written for non-model selections"
        );
    }

    #[tokio::test]
    async fn unknown_config_id_is_not_persisted() {
        let temp = TempDir::new().expect("temp home");

        let persisted = persist_default_model_selection(
            temp.path(),
            "claude-code",
            "not-a-real-option",
            "opus",
            &session_config_options_with_model(),
        )
        .await
        .expect("handle unknown option");

        assert!(!persisted, "unknown options should not be persisted");
        assert!(!temp.path().join("config.toml").exists());
    }
}
