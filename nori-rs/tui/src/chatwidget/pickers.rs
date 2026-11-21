use super::*;

impl ChatWidget {
    pub(crate) fn open_providers_popup(&mut self) {
        // Whether each account is signed in is asked of its own login while the panel is open.
        crate::nori::providers_panel::begin_checks(
            &self.app_event_tx,
            self.frame_requester.clone(),
        );
        self.app_event_tx.send(AppEvent::ShowChoicePicker(Box::new(
            crate::nori::providers_panel::spec(&self.config.nori_home, &self.config.active_agent),
        )));
    }

    pub(crate) fn open_agent_recovery_popup(&mut self, error: &str) {
        let mut params = crate::nori::agent_picker::provider_picker_params();
        params.subtitle = Some(error.to_string());
        self.bottom_pane.show_selection_view(params);
    }

    /// Show a selection view in the bottom pane.
    pub(crate) fn show_selection_view(&mut self, params: SelectionViewParams) {
        self.bottom_pane.show_selection_view(params);
    }

    pub(crate) fn open_reference_session_picker(&mut self) {
        let target = self
            .focused_subagent
            .map(crate::subagent_switcher::Target::Subagent)
            .unwrap_or(crate::subagent_switcher::Target::Main);
        let nori_home = self.config.nori_home.clone();
        let cwd = self.config.cwd.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            match crate::nori::reference_session_picker::load(&nori_home, &cwd).await {
                Ok(sessions) => tx.send(AppEvent::ShowReferenceSessionPicker {
                    sessions,
                    nori_home,
                    target,
                }),
                Err(error) => tx.send(AppEvent::InsertHistoryCell(Box::new(
                    crate::history_cell::new_error_event(format!(
                        "Failed to load sessions: {error}"
                    )),
                ))),
            }
        });
    }

    /// `/resume`: one list of this project's recorded sessions of the active agent and the
    /// sessions the agent itself lists that Nori has no record of. Every row resumes into a
    /// live session.
    pub(crate) fn open_resume_session_picker(&mut self) {
        let Some(handle) = self.harness_handle.clone() else {
            self.app_event_tx
                .send(crate::app_event::AppEvent::OpenAgentSessionPicker);
            return;
        };
        // The agent's own sessions are offered when it advertises ACP `session/list` and can
        // reopen a listed session (`session/load` or `session/resume`).
        let lists_sessions = self.session_agent_capabilities.session_list
            && (self.session_agent_capabilities.load_session
                || self.session_agent_capabilities.session_resume);
        let cwd = self.config.cwd.clone();
        let tx = self.app_event_tx.clone();
        let request = self.resume_picker_request();
        // The records and the agent's listing are read at once; the picker opens with the
        // records and takes the agent's sessions when they come.
        let agent_listing = async move {
            if !lists_sessions {
                return Vec::new();
            }
            match handle.list_sessions(cwd).await {
                Ok(sessions) => sessions,
                Err(error) => {
                    // The records are still worth listing.
                    tracing::warn!("The agent could not list its sessions: {error}");
                    Vec::new()
                }
            }
        };
        crate::latency::begin(crate::latency::Interaction::PickerOpen, false);
        tokio::spawn(request.send_with_agent_listing(tx, agent_listing));
    }

    fn resume_picker_request(&self) -> crate::resume_picker::load::ResumeRequest {
        crate::resume_picker::load::ResumeRequest {
            nori_home: self.config.nori_home.clone(),
            cwd: self.config.cwd.clone(),
            agent: self.config.active_agent.clone(),
            current: self.conversation_id().map(|id| id.to_string()),
        }
    }

    /// Show the resume picker over this project's records of the active agent's sessions and
    /// `agent_sessions`, the ones the agent lists.
    pub(crate) fn show_resume_picker(
        &mut self,
        agent_sessions: Vec<nori_protocol::acp::v1::SessionInfo>,
    ) {
        let request = self.resume_picker_request();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move { request.send(tx, agent_sessions).await });
    }

    /// Show the resume picker over only the sessions an agent lists, with "start a new
    /// session" above them: the agent is not this session's (a candidate being switched to, or
    /// a cloud broker), so Nori's records of other agents' sessions do not apply.
    pub(crate) fn show_agent_session_picker(
        &mut self,
        sessions: Vec<nori_protocol::acp::v1::SessionInfo>,
        cancel_candidate_on_dismiss: bool,
    ) {
        self.app_event_tx
            .send(crate::app_event::AppEvent::ShowResumeSessionPicker(
                Box::new(crate::resume_picker::helix_overlay::ResumeSetup {
                    purpose: crate::resume_picker::helix_overlay::SessionPurpose::Resume,
                    nori_home: self.config.nori_home.clone(),
                    sessions: Vec::new(),
                    agent_sessions: sessions,
                    current: None,
                    offer_new: true,
                    cancel_candidate_on_dismiss,
                    scope: format!("{} sessions", self.config.active_agent),
                    widen: None,
                }),
            ));
    }

    /// Open the Nori CLI settings popup, optionally selecting `focus`'s row.
    ///
    /// Applies the ephemeral per-session loop-count override so the panel
    /// reflects the current in-session value rather than the persisted one.
    pub(crate) fn open_settings_popup(
        &mut self,
        focus: Option<crate::nori::config_picker::SettingsItem>,
    ) {
        let mut config = self.config.clone();
        if let Some(overridden) = self.loop_count_override {
            config.loop_count = overridden;
        }
        let params = crate::nori::config_picker::config_picker_params(
            &config,
            self.app_event_tx.clone(),
            focus,
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Reopen the settings popup with the cursor on the just-changed setting.
    pub(crate) fn reopen_settings_focused(
        &mut self,
        focus: crate::nori::config_picker::SettingsItem,
    ) {
        self.open_settings_popup(Some(focus));
    }

    /// Open the file manager sub-picker.
    pub(crate) fn open_file_manager_picker(&mut self, current: Option<nori_config::FileManager>) {
        let params = crate::nori::config_picker::file_manager_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the vim mode sub-picker. `from_settings` is true when opened from
    /// the `/settings` panel (so it should return there afterward).
    pub(crate) fn open_vim_mode_picker(
        &mut self,
        current: nori_config::VimEnterBehavior,
        from_settings: bool,
    ) {
        let params = crate::nori::config_picker::vim_mode_picker_params(
            current,
            self.app_event_tx.clone(),
            from_settings,
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the auto-worktree sub-picker.
    pub(crate) fn open_auto_worktree_picker(&mut self, current: nori_config::AutoWorktree) {
        let params = crate::nori::config_picker::auto_worktree_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the notify-after-idle sub-picker.
    pub(crate) fn open_notify_after_idle_picker(&mut self, current: nori_config::NotifyAfterIdle) {
        let params = crate::nori::config_picker::notify_after_idle_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the `/browser` profile picker, pre-selected on the saved default.
    #[cfg(unix)]
    pub(crate) fn open_browser_profile_picker(&mut self, current: nori_config::BrowserProfileMode) {
        let params = crate::nori::config_picker::browser_profile_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the script timeout sub-picker.
    pub(crate) fn open_script_timeout_picker(&mut self, current: nori_config::ScriptTimeout) {
        let params = crate::nori::config_picker::script_timeout_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the loop count sub-picker.
    pub(crate) fn open_loop_count_picker(&mut self, current: Option<i32>) {
        self.show_choice(crate::nori::loop_count_picker::loop_count_choices(current));
    }

    /// Show a list of choices described as data, as an overlay.
    pub(crate) fn show_choice(&mut self, spec: crate::choice_picker::ChoiceSpec) {
        self.app_event_tx
            .send(AppEvent::ShowChoicePicker(Box::new(spec)));
    }

    /// Open the footer segments picker popup.
    pub(crate) fn open_footer_segments_picker(
        &mut self,
        current: &nori_config::FooterSegmentConfig,
    ) {
        let params = crate::nori::config_picker::footer_segments_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the worktree choice picker for per-session skillsets.
    pub(crate) fn open_skillset_worktree_choice_picker(&mut self) {
        let params =
            crate::nori::config_picker::skillset_worktree_choice_params(self.app_event_tx.clone());
        self.bottom_pane.show_selection_view(params);
    }

    /// Replace the current footer segments picker with a refreshed one.
    ///
    /// Used after toggling a segment so the picker shows updated state without
    /// stacking a new view on top of the old one.
    pub(crate) fn replace_footer_segments_picker(
        &mut self,
        current: &nori_config::FooterSegmentConfig,
    ) {
        let params = crate::nori::config_picker::footer_segments_picker_params(
            current,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.replace_selection_view(params);
    }

    /// Set a footer segment's enabled state.
    pub(crate) fn set_footer_segment_enabled(
        &mut self,
        segment: nori_config::FooterSegment,
        enabled: bool,
    ) {
        self.bottom_pane
            .set_footer_segment_enabled(segment, enabled);
    }

    /// Set the loop state for a new iteration.
    pub(crate) fn set_loop_state(&mut self, remaining: i32, total: i32) {
        self.loop_remaining = Some(remaining);
        self.loop_total = Some(total);
    }

    /// Set the ephemeral per-session loop count override.
    pub(crate) fn set_loop_count_override(&mut self, value: Option<Option<i32>>) {
        self.loop_count_override = value;
    }

    /// Cancel any active loop.
    pub(super) fn cancel_loop(&mut self) {
        if self.loop_remaining.is_some() {
            tracing::info!(remaining = ?self.loop_remaining, "loop cancelled");
            self.loop_remaining = None;
            self.loop_total = None;
            self.add_info_message("Loop cancelled.".to_string(), None);
        }
    }

    /// Open the MCP server management popup.
    pub(crate) fn open_mcp_servers_popup(&mut self) {
        let servers: std::collections::BTreeMap<String, nori_config::McpServerConfig> = self
            .config
            .mcp_servers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        self.show_choice(crate::nori::mcp_server_picker::mcp_choices(&servers));
        self.app_event_tx
            .send(crate::app_event::AppEvent::ComputeMcpAuthStatuses);
    }

    /// Open the hotkey picker sub-view.
    pub(crate) fn open_hotkey_picker(&mut self, hotkey_config: nori_config::HotkeyConfig) {
        let view = crate::nori::hotkey_picker::HotkeyPickerView::new(
            &hotkey_config,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_view(Box::new(view));
    }

    /// The keymap the `?` list is made from.
    pub(crate) fn set_keymap(&mut self, keymap: nori_config::Keymap) {
        self.bottom_pane.set_keymap(keymap);
    }

    /// Update the hotkey configuration used by the textarea for editing bindings.
    pub(crate) fn set_hotkey_config(&mut self, config: nori_config::HotkeyConfig) {
        self.bottom_pane.set_hotkey_config(config);
    }

    pub(crate) fn set_kakoune_mode(&mut self, enabled: bool) {
        self.bottom_pane.set_kakoune_mode(enabled);
    }

    pub(crate) fn set_vim_mode(&mut self, value: nori_config::VimEnterBehavior) {
        self.bottom_pane.set_vim_mode(value);
    }

    /// Handle the /switch-skillset command.
    /// Checks if nori-skillsets is available and lists available skillsets.
    pub(crate) fn handle_switch_skillset_command(&mut self) {
        use crate::nori::skillset_picker;

        // Check if nori-skillsets is available in PATH
        if !skillset_picker::is_nori_skillsets_available() {
            self.add_info_message(skillset_picker::not_installed_message(), None);
            return;
        }

        // Install into the session's worktree when we're in one; otherwise
        // fall back to the home install rather than polluting the repo root.
        let install_dir = skillset_picker::session_skillset_install_dir(&self.config.cwd);

        // Spawn async task to list skillsets
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            match skillset_picker::list_skillsets().await {
                Ok(names) if names.is_empty() => {
                    tx.send(AppEvent::SkillsetListResult {
                        names: Some(vec![]),
                        error: Some("No skillsets available.".to_string()),
                        install_dir,
                    });
                }
                Ok(names) => {
                    tx.send(AppEvent::SkillsetListResult {
                        names: Some(names),
                        error: None,
                        install_dir,
                    });
                }
                Err(message) => {
                    tx.send(AppEvent::SkillsetListResult {
                        names: None,
                        error: Some(message),
                        install_dir,
                    });
                }
            }
        });
    }

    /// Handle `/switch-skillset <name>` — directly switch to a named skillset
    /// without showing the picker menu.
    pub(crate) fn handle_switch_skillset_command_with_name(&mut self, name: &str) {
        use crate::nori::skillset_picker;

        if !skillset_picker::is_nori_skillsets_available() {
            self.add_info_message(skillset_picker::not_installed_message(), None);
            return;
        }

        let install_dir = skillset_picker::session_skillset_install_dir(&self.config.cwd);

        if let Some(dir) = install_dir {
            self.on_switch_skillset_request(name, &dir);
        } else {
            self.on_install_skillset_request(name);
        }
    }

    /// Handle the result of listing skillsets.
    pub(crate) fn on_skillset_list_result(
        &mut self,
        names: Option<Vec<String>>,
        error: Option<String>,
        install_dir: Option<PathBuf>,
    ) {
        match (names, error) {
            (Some(names), None) if !names.is_empty() => {
                // Determine whether to show "No Skillset" option and worktree warning.
                // These are relevant when skillset_per_session is enabled (indicated
                // by the on_dismiss callback being set, which only happens in the
                // per-session startup flow).
                let is_per_session = self.config.skillset_per_session;
                let auto_worktree_off = is_per_session && !self.config.auto_worktree.is_enabled();

                let on_dismiss: SelectionAction = Box::new(|tx| {
                    tx.send(AppEvent::SkillsetPickerDismissed);
                });
                let spec = crate::nori::skillset_picker::skillset_choices(
                    names,
                    install_dir,
                    Some(on_dismiss),
                    is_per_session,
                    auto_worktree_off,
                );
                self.show_choice(spec);
            }
            (_, Some(error)) => {
                self.add_error_message(error);
            }
            _ => {
                self.add_info_message("No skillsets available.".to_string(), None);
            }
        }
    }

    /// Handle a request to install a skillset.
    pub(crate) fn on_install_skillset_request(&mut self, name: &str) {
        use crate::nori::skillset_picker;

        let name = name.to_string();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            match skillset_picker::install_skillset(&name).await {
                Ok(message) => {
                    tx.send(AppEvent::SkillsetInstallResult {
                        name,
                        success: true,
                        message,
                    });
                }
                Err(message) => {
                    tx.send(AppEvent::SkillsetInstallResult {
                        name,
                        success: false,
                        message,
                    });
                }
            }
        });
    }

    /// Handle the result of installing a skillset.
    pub(crate) fn on_skillset_install_result(&mut self, name: &str, success: bool, message: &str) {
        if success {
            self.add_to_history(history_cell::new_skillset_switched_event(name));
            self.request_redraw();
        } else {
            self.add_error_message(format!("Failed to install skillset '{name}': {message}"));
        }
    }

    /// Handle a request to switch to a skillset with a specific install directory.
    pub(crate) fn on_switch_skillset_request(&mut self, name: &str, install_dir: &std::path::Path) {
        use crate::nori::skillset_picker;

        let name = name.to_string();
        let install_dir = install_dir.to_path_buf();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            match skillset_picker::switch_skillset(&name, &install_dir).await {
                Ok(message) => {
                    tx.send(AppEvent::SkillsetSwitchResult {
                        name,
                        success: true,
                        message,
                    });
                }
                Err(message) => {
                    tx.send(AppEvent::SkillsetSwitchResult {
                        name,
                        success: false,
                        message,
                    });
                }
            }
        });
    }

    /// Handle the result of switching a skillset.
    pub(crate) fn on_skillset_switch_result(&mut self, name: &str, success: bool, message: &str) {
        if success {
            self.add_to_history(history_cell::new_skillset_switched_event(name));
            self.request_redraw();
        } else {
            self.add_error_message(format!("Failed to switch to skillset '{name}': {message}"));
        }
    }

    /// Open the ACP model picker popup.
    ///
    /// Uses the stable `config_options` mechanism: if the agent advertises a
    /// Model-category config option, open the value picker for it. Otherwise
    /// show a static "not supported" message.
    pub(crate) fn open_model_popup(&mut self) {
        // The advertised configuration is already tracked, so the picker opens
        // from it rather than re-fetching what the agent already told us.
        let advertised_model = self
            .agent_config()
            .option_for_category(&nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
            .map(|option| option.raw.clone());
        if let Some(option) = advertised_model {
            self.open_acp_session_config_value_picker(
                option,
                self.focused_subagent
                    .map(crate::nori::session_config_picker::ConfigPickerOrigin::Child)
                    .unwrap_or(crate::nori::session_config_picker::ConfigPickerOrigin::Direct),
            );
            return;
        }

        if self.focused_subagent.is_some() {
            let params = SelectionViewParams {
                title: Some("Model controls unavailable for this child session".to_owned()),
                items: vec![SelectionItem {
                    name: "No advertised models".to_owned(),
                    ..Default::default()
                }],
                ..Default::default()
            }
            .picker();
            self.bottom_pane.show_selection_view(params);
            return;
        }

        if let Some(handle) = self.harness_handle.clone() {
            let app_event_tx = self.app_event_tx.clone();
            tokio::spawn(async move {
                if let Some(config_options) = handle.get_session_config().await {
                    let model_option = config_options.into_iter().find(|opt| {
                        opt.category
                            == Some(nori_protocol::acp::v1::SessionConfigOptionCategory::Model)
                    });
                    if let Some(option) = model_option {
                        app_event_tx.send(AppEvent::OpenAcpSessionConfigValuePicker {
                            option,
                            origin: crate::nori::session_config_picker::ConfigPickerOrigin::Direct,
                        });
                        return;
                    }
                }

                // The agent does not advertise a Model-category config option,
                // so model selection is not supported.
                app_event_tx.send(AppEvent::OpenAcpModelPickerUnsupported);
            });
            return;
        }
        self.open_model_without_session_popup();
    }

    /// Show that choosing a model needs a session, with no agent to read options from yet.
    pub(crate) fn open_model_without_session_popup(&mut self) {
        let mut params = crate::nori::unified_model_picker::params(
            &self.config,
            None,
            None,
            crate::nori::session_config_picker::ConfigPickerOrigin::Direct,
        );
        let items = &mut params.items;
        if items.is_empty() {
            items.push(SelectionItem {
                name: "No models available yet".to_string(),
                description: Some("Connect a provider with /providers and try again.".to_string()),
                ..Default::default()
            });
        }
        self.bottom_pane.show_selection_view(params);
    }

    /// Show the static "model switching not supported" picker.
    pub(crate) fn open_model_unsupported_popup(&mut self) {
        self.open_model_without_session_popup();
    }

    /// Open the generic ACP session-config picker.
    pub(crate) fn open_session_config_popup(&mut self) {
        // Open from the tracked configuration when the agent has advertised
        // one; only an agent that has not yet announced anything needs a fetch.
        if !self.agent_config().is_empty() {
            let config_options = self.agent_config().raw_options();
            self.open_acp_session_config_picker(config_options, None);
            return;
        }

        if let Some(handle) = self.harness_handle.clone() {
            let app_event_tx = self.app_event_tx.clone();
            tokio::spawn(async move {
                let config_options = handle.get_session_config().await.unwrap_or_default();
                app_event_tx.send(AppEvent::OpenAcpSessionConfigPicker {
                    config_options,
                    focus_config_id: None,
                });
            });
            return;
        }

        let params =
            crate::nori::session_config_picker::acp_session_config_picker_params(&[], None);
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the top-level ACP session-config picker with the current config
    /// snapshot, optionally selecting the row named by `focus_config_id`.
    pub(crate) fn open_acp_session_config_picker(
        &mut self,
        config_options: Vec<nori_protocol::acp::v1::SessionConfigOption>,
        focus_config_id: Option<String>,
    ) {
        let params = crate::nori::session_config_picker::acp_session_config_picker_params(
            &config_options,
            focus_config_id.as_deref(),
        );
        self.bottom_pane.show_selection_view(params);
    }

    /// Open the value picker for one ACP session config option.
    pub(crate) fn open_acp_session_config_value_picker(
        &mut self,
        option: nori_protocol::acp::v1::SessionConfigOption,
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
    ) {
        let is_model =
            option.category == Some(nori_protocol::acp::v1::SessionConfigOptionCategory::Model);
        if is_model
            && matches!(
                origin,
                crate::nori::session_config_picker::ConfigPickerOrigin::Direct
                    | crate::nori::session_config_picker::ConfigPickerOrigin::Child(_)
            )
        {
            let params = crate::nori::unified_model_picker::params(
                &self.config,
                Some(&self.config.active_agent),
                Some(&option),
                origin,
            );
            self.bottom_pane.show_selection_view(params);
            return;
        }
        let params = crate::nori::session_config_picker::acp_session_config_value_picker_params(
            &option,
            &[],
            origin,
        );
        self.bottom_pane.show_selection_view(params);
    }

    pub(crate) fn open_custom_model_input(
        &mut self,
        config_id: String,
        option_name: String,
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
    ) {
        let view = crate::nori::custom_model_input::CustomModelInputView::new(
            config_id,
            option_name,
            origin,
            self.app_event_tx.clone(),
        );
        self.bottom_pane.show_view(Box::new(view));
    }

    /// Set an ACP session config option via the agent handle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_acp_session_config_option(
        &mut self,
        config_id: String,
        value: String,
        option_name: String,
        is_custom_model: bool,
        origin: crate::nori::session_config_picker::ConfigPickerOrigin,
        route_token: Option<i64>,
    ) {
        if let Some(handle) = self.harness_handle.clone() {
            if matches!(
                origin,
                crate::nori::session_config_picker::ConfigPickerOrigin::Direct
                    | crate::nori::session_config_picker::ConfigPickerOrigin::Panel
            ) {
                self.show_chosen_option(&config_id, &value);
            }
            let app_event_tx = self.app_event_tx.clone();
            let generation = self.acp_mode_config_generation;
            let agent = self.config.active_agent.clone();
            let option_name_for_result = option_name;
            let config_id_for_result = config_id.clone();
            let value_for_result = value.clone();
            tokio::spawn(async move {
                match handle.set_session_config_option(config_id, value).await {
                    Ok(config_options) => {
                        app_event_tx.send(AppEvent::AcpModeConfigSnapshot {
                            generation,
                            mode: crate::nori::session_config_mode::acp_mode_config_from_options(
                                &config_options,
                            ),
                        });
                        // A value chosen from the `/config` panel returns to the panel on the
                        // edited option, so several settings can be changed in a row. A picker
                        // opened for one setting (`/model`) is done.
                        match origin {
                            crate::nori::session_config_picker::ConfigPickerOrigin::Panel => {
                                app_event_tx.send(AppEvent::OpenAcpSessionConfigPicker {
                                    config_options: config_options.clone(),
                                    focus_config_id: Some(config_id_for_result.clone()),
                                });
                            }
                            crate::nori::session_config_picker::ConfigPickerOrigin::Direct
                            | crate::nori::session_config_picker::ConfigPickerOrigin::Child(_)
                            | crate::nori::session_config_picker::ConfigPickerOrigin::Cycle => {}
                        }
                        app_event_tx.send(AppEvent::AcpSessionConfigSetResult {
                            success: true,
                            agent,
                            config_id: config_id_for_result,
                            value: value_for_result,
                            option_name: option_name_for_result,
                            is_custom_model,
                            origin,
                            config_options: Some(config_options),
                            error: None,
                            route_fallback_eligible: false,
                            route_token,
                        });
                    }
                    Err(err) => {
                        app_event_tx.send(AppEvent::AcpSessionConfigSetResult {
                            success: false,
                            agent,
                            config_id: config_id_for_result,
                            value: value_for_result,
                            option_name: option_name_for_result,
                            is_custom_model,
                            origin,
                            config_options: None,
                            error: Some(err.to_string()),
                            route_fallback_eligible: crate::app::model_routes::eligible_error(&err),
                            route_token,
                        });
                    }
                }
            });
        } else {
            self.add_info_message(
                "No ACP agent handle available for session config".to_string(),
                None,
            );
        }
    }
}
