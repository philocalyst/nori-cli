use super::*;

impl ChatWidget {
    /// Give the composer the prompts of the main conversation, `cells`, for walking back with
    /// `↑`. Cheap when nothing changed: the transcript's length and last cell identify it.
    pub(crate) fn sync_conversation_prompts(
        &mut self,
        cells: &[std::sync::Arc<dyn crate::history_cell::HistoryCell>],
    ) {
        let last = cells
            .last()
            .map_or(0, |cell| std::sync::Arc::as_ptr(cell).cast::<()>() as usize);
        let fingerprint = (cells.len(), last);
        let generation = self.session_generation();
        if self
            .bottom_pane
            .conversation_prompts_current(generation, fingerprint)
        {
            return;
        }
        let prompts = cells
            .iter()
            .enumerate()
            .filter_map(|(cell_index, cell)| {
                cell.as_any()
                    .downcast_ref::<crate::history_cell::UserHistoryCell>()
                    .map(|user| crate::bottom_pane::RecalledPrompt {
                        cell_index,
                        text: user.message.clone(),
                    })
            })
            .collect();
        self.bottom_pane
            .set_conversation_prompts(generation, fingerprint, prompts);
    }

    /// The clipboard's text for the composer's editor (`space p` and friends).
    pub(crate) fn paste_clipboard(&mut self, command: nori_editor::ClipboardCommand, text: &str) {
        self.bottom_pane.paste_clipboard(command, text);
    }

    pub(crate) fn handle_key_event(&mut self, key_event: KeyEvent) {
        // The palette shows what each dial value would do, which depends on whether a turn runs.
        if key_event.kind == KeyEventKind::Press && !self.bottom_pane.has_active_view() {
            self.refresh_action_choices();
            // The plugins' actions as the runtime holds them now (a watch: never stale).
            if let Some(handle) = self.harness_handle() {
                let actions = handle.plugin_actions().borrow().clone();
                self.bottom_pane.set_plugin_actions(actions);
            }
        }
        // Starting to type a prompt is when delegation may soon be asked for: bring what each
        // agent offers up to date, in the background, for agents not read in a while. Tests
        // must never start the real agents installed on the machine.
        #[cfg(not(test))]
        if key_event.kind == KeyEventKind::Press
            && matches!(key_event.code, KeyCode::Char(_))
            && !key_event
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.harness_handle.is_some()
            && self.bottom_pane.composer_is_empty()
            && !self.bottom_pane.has_active_view()
        {
            nori_harness::refresh_agent_catalog_except(
                std::sync::Arc::new(self.config.clone()),
                env!("CARGO_PKG_VERSION").to_string(),
                self.config.active_agent.clone(),
            );
        }
        match key_event {
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'c') => {
                self.on_ctrl_c();
                return;
            }
            KeyEvent {
                code: KeyCode::Char(c),
                modifiers,
                kind: KeyEventKind::Press,
                ..
            } if modifiers.contains(KeyModifiers::CONTROL) && c.eq_ignore_ascii_case(&'v') => {
                match paste_image_to_temp_png() {
                    Ok((path, info)) => {
                        self.attach_image(
                            path,
                            info.width,
                            info.height,
                            info.encoded_format.label(),
                        );
                    }
                    Err(err) => {
                        tracing::warn!("failed to paste image: {err}");
                        self.add_to_history(history_cell::new_error_event(format!(
                            "Failed to paste image: {err}",
                        )));
                    }
                }
                return;
            }
            KeyEvent {
                code: KeyCode::BackTab,
                kind: KeyEventKind::Press,
                ..
            } if !self.exiting
                && self.focused_subagent.is_none()
                && !self.bottom_pane.has_active_overlay_or_popup()
                && self.cycle_acp_mode_config() =>
            {
                return;
            }
            other if other.kind == KeyEventKind::Press => {
                self.bottom_pane.clear_ctrl_c_quit_hint();
            }
            _ => {}
        }

        if let Some(key) = self.focused_subagent
            && key_event.kind == KeyEventKind::Press
            && key_event.code == KeyCode::Enter
            && key_event.modifiers.is_empty()
            && !self.bottom_pane.has_active_overlay_or_popup()
            && self.bottom_pane.composer_is_empty()
        {
            if let (Some(handle), Some(session_id)) =
                (self.harness_handle.clone(), self.subagent_session(key))
            {
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle.promote_subagent_queue(session_id).await {
                        tx.send(AppEvent::TransientNotice(error.to_string()));
                    }
                });
            }
            return;
        }

        match self.bottom_pane.handle_key_event(key_event) {
            InputResult::Submitted(text)
                if self.cloud_mode && !self.session_configured_received =>
            {
                self.bottom_pane.restore_composer_submission(text);
            }
            InputResult::Submitted(text) if self.follow_only_attachment => {
                self.bottom_pane.restore_composer_submission(text);
            }
            InputResult::Submitted(text) if let Some(key) = self.focused_subagent => {
                if let Some(agent_name) = text.strip_prefix("/login ").map(str::trim)
                    && !agent_name.is_empty()
                {
                    self.handle_explicit_login(agent_name);
                    return;
                }
                if self.subagent_session(key).is_some() {
                    self.app_event_tx.send(AppEvent::SendSubagentMessage {
                        key,
                        text,
                        image_paths: self.bottom_pane.take_recent_submission_images(),
                        steer: false,
                    });
                } else {
                    self.bottom_pane.restore_composer_submission(text);
                    self.app_event_tx.send(AppEvent::TransientNotice(
                        "This subagent has no session available for follow-up.".to_owned(),
                    ));
                }
            }
            InputResult::Submitted(text) => {
                // The draft's directives (`⚙ model: opus`) apply before the prompt is sent.
                for invocation in self.bottom_pane.take_directives() {
                    self.run_action(invocation);
                }
                let user_message = UserMessage {
                    text,
                    image_paths: self.bottom_pane.take_recent_submission_images(),
                };
                self.submit_user_message(user_message);
            }
            InputResult::Steer(text) if let Some(key) = self.focused_subagent => {
                if self.subagent_session(key).is_some() {
                    self.app_event_tx.send(AppEvent::SendSubagentMessage {
                        key,
                        text,
                        image_paths: self.bottom_pane.take_recent_submission_images(),
                        steer: true,
                    });
                } else {
                    self.bottom_pane.restore_composer_submission(text);
                    self.app_event_tx.send(AppEvent::TransientNotice(
                        "This child has no session available for steering.".to_owned(),
                    ));
                }
            }
            InputResult::Steer(text) => {
                let user_message = UserMessage {
                    text,
                    image_paths: self.bottom_pane.take_recent_submission_images(),
                };
                self.steer_user_message(user_message);
            }
            InputResult::PromoteQueued => {
                if let Some(key) = self.focused_subagent {
                    if let (Some(handle), Some(session_id)) =
                        (self.harness_handle.clone(), self.subagent_session(key))
                    {
                        let tx = self.app_event_tx.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle.promote_subagent_queue(session_id).await {
                                tx.send(AppEvent::TransientNotice(error.to_string()));
                            }
                        });
                    }
                } else if let Some(handle) = self.harness_handle.clone() {
                    let tx = self.app_event_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle.promote_next_queued().await {
                            tx.send(AppEvent::HarnessActionFailed(error.to_string()));
                        }
                    });
                }
            }
            InputResult::Command(cmd) => {
                if self.focused_subagent.is_some()
                    && !matches!(
                        &cmd,
                        SlashCommand::Quit
                            | SlashCommand::Exit
                            | SlashCommand::Reference
                            | SlashCommand::Model
                            | SlashCommand::Login
                    )
                {
                    self.app_event_tx.send(AppEvent::TransientNotice(
                        "Slash commands are available in the main session. Switch back with Ctrl+Tab."
                            .to_owned(),
                    ));
                } else {
                    self.dispatch_command(cmd);
                }
            }
            InputResult::Action(invocation) => self.run_action(invocation),
            InputResult::None => {}
        }
    }

    pub(crate) fn attach_image(
        &mut self,
        path: PathBuf,
        width: u32,
        height: u32,
        format_label: &str,
    ) {
        tracing::info!(
            "attach_image path={path:?} width={width} height={height} format={format_label}",
        );
        self.bottom_pane
            .attach_image(path, width, height, format_label);
        self.request_redraw();
    }

    pub(super) fn dispatch_command(&mut self, cmd: SlashCommand) {
        // Once exit begins, the session is over: ignore repeated quit
        // commands and refuse everything else instead of racing teardown.
        if self.exiting {
            if !matches!(cmd, SlashCommand::Quit | SlashCommand::Exit) {
                let message = format!("'/{}' is unavailable while exiting.", cmd.command());
                self.add_to_history(history_cell::new_error_event(message));
                self.request_redraw();
            }
            return;
        }
        if !cmd.available_during_task() && self.bottom_pane.is_task_running() {
            let message = format!(
                "'/{}' is disabled while a task is in progress.",
                cmd.command()
            );
            self.add_to_history(history_cell::new_error_event(message));
            self.request_redraw();
            return;
        }
        // While a /close awaits the agent, block session-switching commands:
        // the deferred NewSession would otherwise clobber whatever session the
        // user switched to in the meantime.
        if self.session_close_in_flight
            && matches!(
                cmd,
                SlashCommand::New
                    | SlashCommand::Resume
                    | SlashCommand::ResumeViewonly
                    | SlashCommand::Providers
                    | SlashCommand::Close
            )
        {
            let message = format!("'/{}' is disabled while the session closes.", cmd.command());
            self.add_to_history(history_cell::new_error_event(message));
            self.request_redraw();
            return;
        }
        // Unified availability gate: server-advertised verdicts and
        // client-side cloud/local scoping share one mechanism. /quit and
        // /exit are exempt — they must never be disabled.
        if !matches!(cmd, SlashCommand::Quit | SlashCommand::Exit)
            && !self.ensure_builtin_command_enabled(cmd)
        {
            return;
        }
        match cmd {
            SlashCommand::New => {
                self.app_event_tx.send(AppEvent::NewSession);
            }
            SlashCommand::Resume => {
                self.open_resume_session_picker();
            }
            SlashCommand::ResumeViewonly => {
                self.open_viewonly_session_picker();
            }
            SlashCommand::Reference => {
                self.open_reference_session_picker();
            }
            SlashCommand::Close => {
                let Some(handle) = self.harness_handle.clone() else {
                    self.add_error_message("No live agent connection to close.".to_string());
                    return;
                };
                self.session_close_in_flight = true;
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    match handle.close_session().await {
                        Ok(()) => {
                            tx.send(AppEvent::InsertHistoryCell(Box::new(
                                history_cell::new_info_event(
                                    "Session closed (released).".to_string(),
                                    None,
                                ),
                            )));
                        }
                        Err(e) => {
                            tx.send(AppEvent::SessionCloseFailed {
                                message: format!("{e}"),
                            });
                        }
                    }
                });
            }
            SlashCommand::Init => {
                let init_target = self.config.cwd.join(DEFAULT_PROJECT_DOC_FILENAME);
                if init_target.exists() {
                    let message = format!(
                        "{DEFAULT_PROJECT_DOC_FILENAME} already exists here. Skipping /init to avoid overwriting it."
                    );
                    self.add_info_message(message, None);
                    return;
                }
                const INIT_PROMPT: &str = include_str!("../../prompt_for_init_command.md");
                self.submit_user_message(INIT_PROMPT.to_string().into());
            }
            SlashCommand::Compact => {
                self.clear_token_usage();
                self.app_event_tx.send(AppEvent::HarnessAction(
                    crate::app_event::HarnessAction::Compact,
                ));
            }
            SlashCommand::Providers => {
                self.open_providers_popup();
            }
            SlashCommand::Model => {
                self.open_model_popup();
            }
            SlashCommand::Config => {
                self.open_session_config_popup();
            }
            SlashCommand::Approvals => {
                self.open_approvals_popup();
            }
            SlashCommand::Settings => {
                self.open_settings_popup(None);
            }
            SlashCommand::Vim => self.open_vim_mode_picker(self.config.vim_mode, false),
            SlashCommand::Goal => {
                self.request_thread_goal_status();
            }
            SlashCommand::Move => self.open_move_popup(),
            SlashCommand::Quit | SlashCommand::Exit => {
                self.begin_exit();
            }
            SlashCommand::Login => {
                self.handle_login_command();
            }
            SlashCommand::Logout => {
                self.add_info_message(
                    "To logout, run the agent's logout command directly (e.g., `claude /logout`)"
                        .to_string(),
                    None,
                );
            }
            SlashCommand::Continue => {
                self.continue_unfinished_turn();
            }
            SlashCommand::Undo => {
                self.app_event_tx.send(AppEvent::HarnessAction(
                    crate::app_event::HarnessAction::LoadUndoSnapshots,
                ));
            }
            SlashCommand::Browse => match self.config.file_manager {
                Some(fm) => {
                    self.app_event_tx.send(AppEvent::BrowseFiles(fm));
                }
                None => {
                    self.add_error_message(
                        "No file manager configured. Use /settings to set one.".to_string(),
                    );
                }
            },
            SlashCommand::Diff => {
                self.add_diff_in_progress();
                let tx = self.app_event_tx.clone();
                let dir = self
                    .effective_cwd_tracker
                    .effective_cwd()
                    .cloned()
                    .unwrap_or_else(|| self.config.cwd.clone());
                tokio::spawn(async move {
                    tx.send(AppEvent::DiffResult(get_git_changes(&dir).await));
                });
            }
            SlashCommand::Mention => {
                self.insert_str("@");
            }
            SlashCommand::Status => {
                self.add_status_output();
            }
            SlashCommand::Context => self.add_context_output(),
            SlashCommand::Usage => self.app_event_tx.send(AppEvent::OpenUsage),
            SlashCommand::RemoteControl => {
                self.app_event_tx.send(AppEvent::RemoteControlRequested(
                    crate::remote_control::RemoteControlRequest::EnableLocal,
                ));
            }
            SlashCommand::Memory => {
                self.add_memory_output();
            }
            SlashCommand::FirstPrompt => {
                if let Some(text) = &self.first_prompt_text {
                    self.add_info_message(text.clone(), None);
                } else {
                    self.add_info_message("No prompt has been submitted yet.".to_string(), None);
                }
            }
            SlashCommand::Mcp => {
                self.open_mcp_servers_popup();
            }
            SlashCommand::SwitchSkillset => {
                self.handle_switch_skillset_command();
            }
            SlashCommand::Fork => {
                self.app_event_tx.send(AppEvent::OpenForkPicker);
            }
            #[cfg(unix)]
            SlashCommand::Browser => {
                if let Some((ws_url, cdp_port)) =
                    nori_harness::backend::browser_session::active_session_info()
                {
                    self.add_info_message(
                        format!("Browser already running on CDP port {cdp_port} ({ws_url})"),
                        None,
                    );
                    return;
                }
                // Open the profile picker, pre-selected on the saved default.
                let current = nori_config::NoriConfig::load()
                    .map(|config| config.browser_profile)
                    .unwrap_or_default();
                self.open_browser_profile_picker(current);
            }
            #[cfg(not(unix))]
            SlashCommand::Browser => {
                self.add_info_message(
                    "/browser is only available on Unix systems".to_string(),
                    None,
                );
            }
        }
    }

    /// Spawn a Chrome session for the chosen profile `mode` and notify the agent
    /// once CDP is ready. Invoked after the user picks a tier in `/browser`.
    #[cfg(unix)]
    pub(crate) fn launch_browser_session(&mut self, mode: nori_config::BrowserProfileMode) {
        self.add_info_message("Launching browser...".to_string(), None);
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            match nori_harness::backend::browser_session::BrowserSession::launch_and_store(mode)
                .await
            {
                Ok((ws_url, cdp_port)) => {
                    tx.send(AppEvent::BrowserLaunched { ws_url, cdp_port });
                }
                Err(err) => {
                    tx.send(AppEvent::BrowserLaunchFailed(format!("{err:#}")));
                }
            }
        });
    }

    pub(crate) fn handle_paste(&mut self, text: String) {
        self.bottom_pane.handle_paste(text);
    }

    // Returns true if caller should skip rendering this frame (a future frame is scheduled).
    pub(crate) fn handle_paste_burst_tick(&mut self, frame_requester: FrameRequester) -> bool {
        if self.bottom_pane.flush_paste_burst_if_due() {
            // What was held back (a typed character, or a paste) is in the composer now: draw it
            // in this frame. Asking for another one would wait out the frame interval as well.
            false
        } else if self.bottom_pane.is_in_paste_burst() {
            // While capturing a burst, schedule a follow-up tick and skip this frame
            // to avoid redundant renders between ticks.
            frame_requester.schedule_frame_in(
                crate::bottom_pane::ChatComposer::recommended_paste_flush_delay(),
            );
            true
        } else {
            false
        }
    }
}
