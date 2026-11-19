//! Settings, their pickers and the browser.

use super::*;

impl App {
    pub(super) async fn handle_config_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::SetConfigVerticalFooter(enabled) => {
                self.persist_config_setting("vertical_footer", enabled)
                    .await;
            }
            AppEvent::SetConfigTerminalNotifications(enabled) => {
                self.persist_notification_setting("terminal_notifications", enabled)
                    .await;
            }
            AppEvent::SetConfigOsNotifications(enabled) => {
                self.persist_notification_setting("os_notifications", enabled)
                    .await;
            }
            AppEvent::SetConfigHotkey { action, binding } => {
                self.persist_hotkey_setting(action, binding).await;
            }
            AppEvent::OpenHotkeyPicker => {
                self.chat_widget
                    .open_hotkey_picker(self.hotkey_config.clone());
            }
            AppEvent::OpenNotifyAfterIdlePicker => {
                self.chat_widget
                    .open_notify_after_idle_picker(self.config.notify_after_idle);
            }
            AppEvent::SetConfigNotifyAfterIdle(value) => {
                self.persist_notify_after_idle_setting(value).await;
            }
            AppEvent::OpenScriptTimeoutPicker => {
                self.chat_widget
                    .open_script_timeout_picker(self.config.script_timeout.clone());
            }
            AppEvent::SetConfigScriptTimeout(value) => {
                self.persist_script_timeout_setting(value).await;
            }
            AppEvent::OpenLoopCountPicker => {
                let current = match self.loop_count_override {
                    Some(overridden) => overridden,
                    None => self.config.loop_count,
                };
                self.chat_widget.open_loop_count_picker(current);
            }
            AppEvent::SetConfigLoopCount(value) => {
                self.set_session_loop_count(value);
            }
            AppEvent::OpenVimModePicker => {
                self.chat_widget
                    .open_vim_mode_picker(self.config.vim_mode, true);
            }
            AppEvent::OpenAutoWorktreePicker => {
                self.chat_widget
                    .open_auto_worktree_picker(self.config.auto_worktree);
            }
            AppEvent::SetConfigAutoWorktree(value) => {
                self.persist_auto_worktree_setting(value).await;
            }
            AppEvent::SetConfigSkillsetPerSession(enabled) => {
                self.persist_skillset_per_session_setting(enabled).await;
            }
            AppEvent::SetConfigPinnedPlanDrawer(enabled) => {
                self.persist_pinned_plan_drawer_setting(enabled).await;
            }
            AppEvent::SetConfigAcpWireRecording(enabled) => {
                self.persist_acp_wire_recording_setting(enabled).await;
            }
            AppEvent::SetConfigCustomWorkingMessages(enabled) => {
                self.persist_custom_working_messages_setting(enabled).await;
            }
            AppEvent::OpenSkillsetPerSessionWorktreeChoice => {
                self.chat_widget.open_skillset_worktree_choice_picker();
            }
            AppEvent::OpenFooterSegmentsPicker => {
                self.chat_widget
                    .open_footer_segments_picker(&self.footer_segment_config);
            }
            AppEvent::SetConfigFooterSegment(segment, enabled) => {
                self.persist_footer_segment_setting(segment, enabled).await;
            }
            AppEvent::BrowseFiles(fm) => {
                self.browse_files(fm, tui);
            }
            AppEvent::SetConfigFileManager(value) => {
                self.persist_file_manager_setting(value).await;
            }
            AppEvent::OpenFileManagerPicker => {
                self.chat_widget
                    .open_file_manager_picker(self.config.file_manager);
            }
            AppEvent::SetConfigVimMode {
                value,
                from_settings,
            } => {
                self.persist_vim_mode_setting(value, from_settings).await;
            }
            AppEvent::BrowserLaunched { ws_url, cdp_port } => {
                let prompt =
                    nori_harness::backend::browser_session::compose_agent_prompt(&ws_url, cdp_port);
                self.chat_widget.add_info_message(
                    format!("Browser launched (CDP port {cdp_port}). Notifying agent..."),
                    None,
                );
                self.chat_widget.submit_user_message_text(prompt);
            }
            AppEvent::BrowserLaunchFailed(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to launch browser: {err}"));
            }
            #[cfg(unix)]
            AppEvent::SetBrowserProfile(mode) => {
                // Persist the choice as the new default, then launch with it.
                self.persist_browser_profile_setting(mode).await;
                self.chat_widget.launch_browser_session(mode);
            }
            #[cfg(not(unix))]
            AppEvent::SetBrowserProfile(_mode) => {}
            // usage: the overlay reads the store on its own task.
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
