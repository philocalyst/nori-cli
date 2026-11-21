use super::*;

/// Cloud detach gets a short opportunity to finish its stdin-EOF path. Local
/// agents are torn down immediately; they have no remote detach work to flush.
const CLOUD_EXIT_CHILD_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

impl ChatWidget {
    pub(crate) fn set_transcript_tools(&mut self, tools: Vec<super::tool_projection::ToolSpan>) {
        if let Some(transcript) = &mut self.focused_transcript {
            transcript.set_tools(tools);
        }
    }

    pub(crate) fn transcript_tool_at(&self, column: u16, row: u16) -> Option<String> {
        let pos = ratatui::layout::Position::new(column, row);
        if let Some((_, call_id)) = self
            .active_tool_hits
            .borrow()
            .iter()
            .find(|(rect, _)| rect.contains(pos))
        {
            return Some(call_id.clone());
        }
        self.focused_transcript.as_ref()?.tool_at(column, row)
    }

    pub(crate) fn nearest_transcript_tool(&self) -> Option<String> {
        self.focused_transcript.as_ref()?.nearest_tool()
    }
    pub(crate) fn set_main_history_view(
        &mut self,
        lines: Vec<ratatui::text::Line<'static>>,
        scroll_offset: usize,
    ) {
        self.focused_transcript = Some(super::session_transcript::SessionTranscript::new(
            lines,
            scroll_offset,
        ));
    }

    pub(crate) fn set_transcript_follow_live(&mut self, follow: bool) {
        if let Some(transcript) = &mut self.focused_transcript {
            transcript.set_follow_live(follow);
        }
    }

    pub(crate) fn clear_main_history_view(&mut self) {
        self.focused_transcript = None;
    }

    pub(crate) fn set_focused_subagent_view(
        &mut self,
        key: Option<crate::subagent_switcher::SubagentKey>,
        lines: Vec<ratatui::text::Line<'static>>,
        scroll_offset: usize,
    ) {
        if self.focused_subagent != key {
            self.bottom_pane.set_child_context(key.is_some());
            let queued = key
                .and_then(|key| self.subagent_session(key))
                .and_then(|id| self.child_queued_prompts.get(&id).cloned())
                .unwrap_or_else(|| {
                    if key.is_some() {
                        Vec::new()
                    } else {
                        self.main_queued_prompts.clone()
                    }
                });
            self.bottom_pane.set_queued_user_messages(queued);
        }
        self.focused_subagent = key;
        self.focused_transcript =
            key.map(|_| super::session_transcript::SessionTranscript::new(lines, scroll_offset));
    }

    pub(crate) fn focused_subagent(&self) -> Option<crate::subagent_switcher::SubagentKey> {
        self.focused_subagent
    }

    pub(crate) fn restore_composer_submission(&mut self, text: String) {
        self.bottom_pane.restore_composer_submission(text);
    }

    /// Set the vertical footer layout flag for the TUI.
    pub(crate) fn set_vertical_footer(&mut self, enabled: bool) {
        self.bottom_pane.set_vertical_footer(enabled);
    }

    pub(crate) fn set_custom_working_messages(&mut self, enabled: bool) {
        self.config.custom_working_messages = enabled;
        self.bottom_pane.set_custom_working_messages(enabled);
    }

    /// Set the plan drawer mode. The latest plan state is always retained so
    /// that switching to a visible mode shows the most recent plan immediately.
    pub(crate) fn set_plan_drawer_mode(&mut self, mode: PlanDrawerMode) {
        self.plan_drawer_mode = mode;
    }

    /// Cycle the plan drawer: Off -> Collapsed -> Expanded -> Collapsed -> ...
    pub(crate) fn toggle_plan_drawer(&mut self) {
        self.plan_drawer_mode = match self.plan_drawer_mode {
            PlanDrawerMode::Off => PlanDrawerMode::Collapsed,
            PlanDrawerMode::Collapsed => PlanDrawerMode::Expanded,
            PlanDrawerMode::Expanded => PlanDrawerMode::Collapsed,
        };
    }

    /// Return the current plan drawer mode.
    pub(crate) fn plan_drawer_mode(&self) -> PlanDrawerMode {
        self.plan_drawer_mode
    }

    pub(crate) fn add_info_message(&mut self, message: String, hint: Option<String>) {
        self.push_notice_with_hint(history_cell::NoticeKind::Info, message, hint);
    }

    /// The agent capability view from the latest SessionCapabilitiesChanged.
    pub(crate) fn agent_capabilities(&self) -> crate::presentation::AgentCapabilitiesView {
        self.session_agent_capabilities.clone()
    }

    /// The cloud session identity. Id presence IS the cloud signal inside the
    /// widget: `on_session_started` retains it only when the top-level CLI
    /// launched handroll in cloud mode.
    pub(crate) fn cloud_session_identity(&self) -> Option<CloudSessionInfo> {
        self.acp_session_id.as_ref().map(|id| CloudSessionInfo {
            id: id.clone(),
            title: self.cloud_session_title.clone(),
        })
    }

    /// Push the current cloud identity (or its absence) into the footer.
    /// Called from SessionConfigured — the only event that changes the id.
    pub(super) fn refresh_cloud_session_indicator(&mut self) {
        let cloud_session = self.cloud_session_identity().map(|info| info.id);
        self.bottom_pane.set_cloud_session(cloud_session);
    }

    /// A `/close` failed: surface the (already enhanced) error and unblock the
    /// session-switching commands that were held while the close was in flight.
    pub(crate) fn on_session_close_failed(&mut self, message: String) {
        self.session_close_in_flight = false;
        self.add_error_message(format!("Failed to close the session: {message}"));
    }

    /// Begin exiting: immediate feedback, input refused, and bounded backend
    /// cleanup. Idempotent.
    ///
    /// In cloud mode, exit is a *detach*: the connection drop is non-terminal
    /// and the session keeps running server-side, so the feedback says exactly
    /// that.
    pub(crate) fn begin_exit(&mut self) {
        if self.exiting {
            return;
        }
        self.exiting = true;
        if self.stream_ending {
            self.flush_answer_stream_with_separator();
        }
        self.bottom_pane.show_exit_in_progress();

        if self.cloud_mode && self.acp_session_id.is_some() {
            self.add_to_history(history_cell::new_info_event(
                "This session keeps running in the cloud.".to_string(),
                Some("reattach later from the `nori cloud` picker".to_string()),
            ));
        }
        self.request_redraw();

        let child_grace = if self.cloud_mode {
            CLOUD_EXIT_CHILD_GRACE
        } else {
            std::time::Duration::ZERO
        };
        self.submit_harness_action(crate::app_event::HarnessAction::Shutdown { child_grace });
    }

    pub(crate) fn add_plain_history_lines(&mut self, lines: Vec<Line<'static>>) {
        self.add_boxed_history(Box::new(PlainHistoryCell::new(lines)));
        self.request_redraw();
    }

    pub(crate) fn add_error_message(&mut self, message: String) {
        self.push_notice(history_cell::NoticeKind::Error, message);
    }

    pub(crate) fn add_warning_message(&mut self, message: String) {
        self.push_notice(history_cell::NoticeKind::Warning, message);
    }

    /// Queue a plain text message to be submitted as a user turn. If no task
    /// is currently running the message is submitted immediately; otherwise
    /// it is appended to the pending queue.
    pub(crate) fn queue_text_as_user_message(&mut self, text: String) {
        self.submit_user_message(UserMessage::from(text));
    }

    /// Show "Connecting to [Agent]" status indicator during agent startup.
    ///
    /// Called when an ACP agent is being spawned and may take time
    /// (e.g., npx/bunx resolving dependencies).
    pub(crate) fn show_connecting_status(&mut self, display_name: &str) {
        if self.cloud_mode {
            self.add_info_message(format!("Connecting to {display_name}…"), None);
        }
        let header = format!("Connecting to {display_name}");
        self.bottom_pane.ensure_status_indicator();
        self.bottom_pane.set_interrupt_hint_visible(false); // Can't interrupt during connect
        self.set_status_header(header);
        self.request_redraw();
    }

    pub(crate) fn on_agent_spawn_failed(&mut self, agent_name: &str, error: &str) {
        self.bottom_pane.hide_status_indicator();
        self.add_error_message(format!("Failed to start agent '{agent_name}': {error}"));
        self.open_agent_recovery_popup(error);
    }

    pub(crate) fn add_memory_output(&mut self) {
        let files = crate::nori::session_header::active_instruction_file_contents(
            &self.config.active_agent,
            &self.config.cwd,
        );

        if files.is_empty() {
            self.add_info_message("No active instruction files found.".to_string(), None);
            return;
        }

        let mut lines: Vec<Line<'static>> = vec!["/memory".magenta().into()];

        for (path, contents) in files {
            let display_path = crate::nori::session_header::format_instruction_path(&path);
            lines.push(Line::from(""));
            lines.push(Line::from(display_path.bold()));
            for line in contents.lines() {
                lines.push(Line::from(line.to_string().dim()));
            }
        }

        self.add_plain_history_lines(lines);
    }

    /// Forward file-search results to the bottom pane.
    pub(crate) fn apply_file_search_result(&mut self, query: String, matches: Vec<FileMatch>) {
        self.bottom_pane.on_file_search_result(query, matches);
    }

    /// Fill the footer with the system facts that are cheap to read, so the
    /// first frame already shows them instead of leaving every segment blank
    /// until the background refresh lands.
    pub(crate) fn seed_system_info(&mut self) {
        self.bottom_pane.seed_system_info(&self.config.cwd);
    }

    /// Update system info in the footer (for background refresh).
    pub(crate) fn apply_system_info_refresh(&mut self, info: crate::system_info::SystemInfo) {
        self.bottom_pane.set_system_info(info);
    }

    /// The prompt walked back to, as edited in place in the conversation.
    pub(crate) fn inline_walk(&self, width: u16) -> Option<crate::stage::InlineEdit> {
        self.bottom_pane.inline_walk(width)
    }

    pub(crate) fn composer_text(&self) -> String {
        self.bottom_pane.composer_text()
    }

    /// Move the full edit, including buffered input, before replacing this widget.
    pub(crate) fn take_composer_draft(&mut self) -> crate::bottom_pane::ComposerDraft {
        self.bottom_pane.take_composer_draft()
    }

    pub(crate) fn restore_composer_draft(&mut self, draft: crate::bottom_pane::ComposerDraft) {
        self.bottom_pane.restore_composer_draft(draft);
    }

    pub(crate) fn take_initial_input(&mut self) -> (Option<String>, Vec<PathBuf>) {
        let Some(UserMessage { text, image_paths }) = self.initial_user_message.take() else {
            return (None, Vec::new());
        };
        (Some(text).filter(|text| !text.is_empty()), image_paths)
    }

    pub(crate) fn has_harness_session(&self) -> bool {
        self.harness_handle.is_some()
    }

    pub(crate) fn loop_state(&self) -> Option<(i32, i32)> {
        self.loop_remaining.zip(self.loop_total)
    }

    pub(crate) fn submit_launch_input(
        &mut self,
        prompt: Option<String>,
        image_paths: Vec<PathBuf>,
    ) {
        let Some(message) =
            super::create_initial_user_message(prompt.clone().unwrap_or_default(), image_paths)
        else {
            return;
        };
        if self.first_prompt_text.is_none() {
            self.first_prompt_text = prompt;
        }
        self.submit_user_message(message);
    }

    /// Returns true if a popup or custom view is currently active in the bottom pane.
    pub(crate) fn handle_popup_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> bool {
        self.bottom_pane.handle_popup_mouse(mouse)
    }

    pub(crate) fn has_active_popup(&self) -> bool {
        self.bottom_pane.has_active_view()
    }

    pub(crate) fn has_active_overlay_or_popup(&self) -> bool {
        self.bottom_pane.has_active_overlay_or_popup()
    }

    pub(crate) fn composer_caret_on_first_row(&self) -> bool {
        self.bottom_pane.composer_caret_on_first_row()
    }

    pub(crate) fn composer_is_empty(&self) -> bool {
        self.bottom_pane.composer_is_empty()
    }

    pub(crate) fn is_composer_in_vim_normal_mode(&self) -> bool {
        self.bottom_pane.is_composer_in_vim_normal_mode()
    }

    pub(crate) fn submit_user_prompt_text(&mut self, text: String) {
        self.submit_user_message(text.into());
    }

    /// True when the UI is in the regular composer state with no running task,
    /// no modal overlay (e.g. approvals or status indicator), and no composer popups.
    /// In this state Esc-Esc backtracking is enabled.
    pub(crate) fn is_normal_backtrack_mode(&self) -> bool {
        self.bottom_pane.is_normal_backtrack_mode()
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        self.bottom_pane.insert_str(text);
    }

    /// Replace the composer content, placing the cursor at the end.
    /// For widget handoffs, transfer a composer draft instead.
    pub(crate) fn set_composer_text(&mut self, text: String) {
        self.bottom_pane.set_composer_text(text);
    }

    pub(crate) fn shutdown_harness_session(&self) {
        let Some(handle) = self.harness_handle.clone() else {
            return;
        };
        let child_grace = if self.cloud_mode {
            CLOUD_EXIT_CHILD_GRACE
        } else {
            std::time::Duration::ZERO
        };
        tokio::spawn(async move {
            if let Err(error) = handle.shutdown_with_grace(child_grace).await {
                tracing::warn!(%error, "failed to shut down replaced harness session");
            }
        });
    }

    pub(crate) fn harness_handle(&self) -> Option<HarnessHandle> {
        self.harness_handle.clone()
    }

    pub(crate) fn submit_harness_action(&self, action: crate::app_event::HarnessAction) {
        let Some(handle) = self.harness_handle.clone() else {
            if let crate::app_event::HarnessAction::SendSubagentMessage {
                key,
                text,
                image_paths,
                ..
            } = action
            {
                self.app_event_tx.send(AppEvent::SubagentMessageFailed {
                    key,
                    text,
                    image_paths,
                    error: "No active harness session".to_owned(),
                });
            } else if matches!(action, crate::app_event::HarnessAction::Shutdown { .. }) {
                self.app_event_tx.send(AppEvent::ExitRequest);
            } else {
                self.app_event_tx.send(AppEvent::HarnessActionFailed(
                    "No active harness session".to_string(),
                ));
            }
            return;
        };
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            use crate::app_event::HarnessAction;
            let result = match action {
                HarnessAction::Cancel => handle.cancel().await,
                HarnessAction::StopSubagents => handle.stop_subagents().await,
                HarnessAction::StopAgent => handle.stop_agent().await,
                HarnessAction::Shutdown { child_grace } => {
                    let result = handle.shutdown_with_grace(child_grace).await;
                    if result.is_err() {
                        app_event_tx.send(AppEvent::ExitRequest);
                    }
                    result
                }
                HarnessAction::Compact => handle.compact().await,
                HarnessAction::Branch => handle.branch().await,
                HarnessAction::ForkHistory { context, source } => {
                    handle.fork_history(context, source).await
                }
                HarnessAction::SendSubagentMessage {
                    key,
                    session_id,
                    text,
                    image_paths,
                    steer,
                } => {
                    let mut content = Vec::new();
                    if !text.is_empty() {
                        content.push(nori_protocol::acp::v1::ContentBlock::Text(
                            nori_protocol::acp::v1::TextContent::new(text.clone()),
                        ));
                    }
                    let images = image_paths
                        .iter()
                        .map(|path| super::user_input::local_image_content(path))
                        .collect::<Result<Vec<_>, _>>();
                    let result = match images {
                        Ok(images) => {
                            content.extend(images);
                            if steer {
                                handle
                                    .steer_subagent_content(session_id, content)
                                    .await
                                    .map_err(|error| error.to_string())
                            } else {
                                handle
                                    .send_subagent_content(session_id, content)
                                    .await
                                    .map_err(|error| error.to_string())
                            }
                        }
                        Err(error) => Err(error),
                    };
                    match result {
                        Ok(()) => Ok(()),
                        Err(error) => {
                            app_event_tx.send(AppEvent::SubagentMessageFailed {
                                key,
                                text,
                                image_paths,
                                error,
                            });
                            Ok(())
                        }
                    }
                }
                HarnessAction::UndoTo(index) => handle.undo_to(index).await,
                HarnessAction::LoadUndoSnapshots => match handle.undo_snapshots().await {
                    Ok(snapshots) => {
                        app_event_tx.send(AppEvent::UndoSnapshotsLoaded(snapshots));
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
                HarnessAction::RunUserShell(command) => handle.run_user_shell(command).await,
                HarnessAction::AddHistory(text) => handle.add_history(text).await,
                HarnessAction::HistoryEntry { log_id, offset } => {
                    match handle.history_entry(log_id, offset).await {
                        Ok(entry) => {
                            app_event_tx.send(AppEvent::HistoryEntryLoaded {
                                log_id,
                                offset,
                                entry,
                            });
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                HarnessAction::SearchHistory { max_results } => {
                    match handle.search_history(max_results).await {
                        Ok(entries) => {
                            app_event_tx.send(AppEvent::HistorySearchLoaded(entries));
                            Ok(())
                        }
                        Err(error) => Err(error),
                    }
                }
                HarnessAction::LoadCustomPrompts => match handle.custom_prompts().await {
                    Ok(prompts) => {
                        app_event_tx.send(AppEvent::CustomPromptsLoaded(prompts));
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
                HarnessAction::LoadGoal => match handle.goal().await {
                    Ok(goal) => {
                        app_event_tx.send(AppEvent::GoalLoaded(goal));
                        Ok(())
                    }
                    Err(error) => Err(error),
                },
                HarnessAction::SetGoal { objective, status } => {
                    handle.set_goal(objective, status).await.map(drop)
                }
                HarnessAction::SetGoalStatus(status) => {
                    handle.set_goal_status(status).await.map(drop)
                }
                HarnessAction::ClearGoal => handle.clear_goal().await,
                HarnessAction::RespondToAgent {
                    request_id,
                    response,
                } => handle.respond_to_agent(request_id, *response).await,
            };
            if let Err(error) = result {
                app_event_tx.send(AppEvent::HarnessActionFailed(error.to_string()));
            }
        });
    }

    /// Hand the prompt to the harness from a task, so reading and encoding image
    /// attachments and waiting for the harness never block the UI.
    pub(crate) fn submit_prompt(
        &self,
        text: String,
        image_paths: Vec<PathBuf>,
        echo: nori_protocol::PromptEchoId,
        steer: bool,
    ) {
        let Some(handle) = self.harness_handle.clone() else {
            self.app_event_tx.send(AppEvent::PromptSubmitFailed(
                "No active harness session".to_string(),
            ));
            return;
        };
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let mut content = Vec::new();
            if !text.is_empty() {
                content.push(nori_protocol::acp::v1::ContentBlock::Text(
                    nori_protocol::acp::v1::TextContent::new(text),
                ));
            }
            if !image_paths.is_empty() {
                let images = tokio::task::spawn_blocking(move || {
                    image_paths
                        .iter()
                        .map(|path| super::user_input::local_image_content(path))
                        .collect::<Result<Vec<_>, String>>()
                })
                .await;
                match images {
                    Ok(Ok(images)) => content.extend(images),
                    Ok(Err(error)) => {
                        app_event_tx.send(AppEvent::PromptSubmitFailed(error));
                        return;
                    }
                    Err(error) => {
                        app_event_tx.send(AppEvent::PromptSubmitFailed(error.to_string()));
                        return;
                    }
                }
            }
            let result = if steer {
                handle.steer_echoed(content, &echo).await
            } else {
                handle.prompt_echoed(content, &echo).await
            };
            if let Err(error) = result {
                app_event_tx.send(AppEvent::PromptSubmitFailed(error.to_string()));
            }
        });
    }

    pub(crate) fn token_usage(&self) -> crate::ui_types::TokenUsage {
        crate::ui_types::TokenUsage::default()
    }

    pub(crate) fn conversation_id(&self) -> Option<ConversationId> {
        self.conversation_id
    }

    /// Return a reference to the widget's current config (includes any
    /// runtime overrides applied via TUI, e.g., model or approval policy).
    pub(crate) fn config_ref(&self) -> &Config {
        &self.config
    }

    /// Replace the runtime config snapshot after an in-TUI edit.
    pub(crate) fn set_config(&mut self, config: Config) {
        self.config = config;
    }

    /// Get a reference to the session statistics tracker.
    pub(crate) fn session_stats(&self) -> &SessionStats {
        &self.session_stats
    }

    pub(crate) fn clear_token_usage(&mut self) {}

    pub(super) fn as_renderable(&self) -> RenderableItem<'_> {
        let mut flex = FlexRenderable::new();
        self.active_tool_hits.borrow_mut().clear();
        if let Some(transcript) = &self.focused_transcript {
            if self.focused_subagent().is_none() && transcript.follows_live() {
                flex.push(
                    1,
                    RenderableItem::Owned(Box::new(super::owned_transcript::OwnedTranscript {
                        transcript,
                        active: self.active_cell.is_some().then_some(&self.active_cell),
                        live: self.live_tail.is_some().then_some(&self.live_tail),
                        live_separator: self.live_tail_has_header,
                    })),
                );
            } else {
                flex.push(
                    1,
                    RenderableItem::Borrowed(transcript).inset(Insets::tlbr(1, 0, 0, 0)),
                );
            }
        } else {
            let active_call_id = self
                .active_cell
                .as_ref()
                .and_then(|cell| cell.as_any().downcast_ref::<ClientToolCell>())
                .map(|tool| tool.call_id().to_owned());
            let active_cell_renderable = if self.active_cell.is_some() {
                if let Some(call_id) = active_call_id {
                    RenderableItem::Owned(Box::new(TrackedActiveTool {
                        inner: &self.active_cell,
                        call_id,
                        hits: &self.active_tool_hits,
                    }))
                    .inset(Insets::tlbr(1, 0, 0, 0))
                } else {
                    RenderableItem::Borrowed(&self.active_cell).inset(Insets::tlbr(1, 0, 0, 0))
                }
            } else {
                RenderableItem::Owned(Box::new(()))
            };
            flex.push(1, active_cell_renderable);
            if self.live_tail.is_some() {
                // The first line of a message sits under a blank separator, like its cell in history.
                let separator = u16::from(self.live_tail_has_header);
                flex.push(
                    1,
                    RenderableItem::Owned(Box::new(super::layout_cache::Capped {
                        cell: &self.live_tail,
                        bottom_pane: &self.bottom_pane,
                        screen_rows: self.screen_rows.get(),
                    }))
                    .inset(Insets::tlbr(separator, 0, 0, 0)),
                );
            }
        }
        // A provider's sign-in program, live, right above the prompt.
        if let Some(terminal) = &self.login_terminal {
            flex.push(0, RenderableItem::Borrowed(terminal));
        }
        // Live subagent cards render directly in the chat stream (never below the prompt bar).
        if self.focused_subagent.is_none() && self.subagents.has_running() {
            flex.push(
                0,
                RenderableItem::Owned(Box::new(subagents::InChatSubagents {
                    tree: &self.subagents,
                    focused: self.focused_subagent,
                    now: Instant::now(),
                    screen_rows: self.screen_rows.get(),
                    hits: &self.subagent_card_hits,
                }))
                .inset(Insets::tlbr(1, 0, 0, 0)),
            );
        } else {
            self.subagent_card_hits.borrow_mut().clear();
        }
        // Pinned plan drawer: renders the latest plan state between the chat stream
        // and the bottom pane.
        self.pinned_plan_area.set(None);
        if let Some(plan) = &self.pinned_plan {
            match self.plan_drawer_mode {
                PlanDrawerMode::Collapsed => {
                    flex.push(
                        0,
                        RenderableItem::Owned(Box::new(TrackedArea {
                            inner: crate::pinned_plan_drawer::PinnedPlanDrawerCollapsed::new(plan),
                            area: &self.pinned_plan_area,
                        }))
                        .inset(Insets::tlbr(1, 0, 0, 0)),
                    );
                }
                PlanDrawerMode::Expanded => {
                    flex.push(
                        0,
                        RenderableItem::Owned(Box::new(TrackedArea {
                            inner: crate::pinned_plan_drawer::PinnedPlanDrawer::new(plan),
                            area: &self.pinned_plan_area,
                        }))
                        .inset(Insets::tlbr(1, 0, 0, 0)),
                    );
                }
                PlanDrawerMode::Off => {}
            }
        }
        // The composer and a view each have their own blank row above them.
        flex.push(
            0,
            RenderableItem::Owned(Box::new(super::layout_cache::PositionedBottomPane {
                pane: &self.bottom_pane,
                area: &self.bottom_pane_area,
            })),
        );
        RenderableItem::Owned(Box::new(flex))
    }

    // --- Terminal title management ---

    /// Returns the project name derived from the working directory.
    fn project_name(&self) -> String {
        self.config
            .cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default()
    }

    /// Whether the terminal title spinner should animate right now.
    pub(crate) fn should_animate_terminal_title_spinner(&self) -> bool {
        self.config.animations && self.terminal_title_has_active_progress()
    }

    /// Whether there is active progress that warrants showing the spinner.
    fn terminal_title_has_active_progress(&self) -> bool {
        self.bottom_pane.is_task_running()
    }

    /// Recompute and write the terminal title. Schedules the next animation
    /// frame if the spinner is active.
    pub(crate) fn refresh_terminal_title(&mut self) {
        // The tab names the project and the conversation, the same title the footer and the
        // pickers show: activity shows in the prompt bar.
        let spinner_frame: Option<&str> = None;

        let project = self.project_name();
        if project.is_empty() {
            return;
        }
        let session = self.session_info_state.title().map(|title| {
            crate::nori::session_info::sanitize(
                title,
                crate::nori::session_info::MAX_TITLE_DISPLAY_CHARS,
            )
            .trim()
            .to_owned()
        });
        let label = match session.filter(|title| !title.is_empty()) {
            Some(title) => format!("{project} · {title}"),
            None => project,
        };

        let title = crate::terminal_title::compose_title(&label, spinner_frame);

        // Skip redundant writes.
        if self.last_terminal_title.as_deref() == Some(&title) {
            // Still schedule the next frame so the animation continues.
            if spinner_frame.is_some() {
                self.frame_requester
                    .schedule_frame_in(crate::terminal_title::SPINNER_INTERVAL);
            }
            return;
        }

        if let Err(err) = crate::terminal_title::set_terminal_title(&title) {
            tracing::debug!(error = %err, "failed to set terminal title");
        }
        self.last_terminal_title = Some(title);

        if spinner_frame.is_some() {
            self.frame_requester
                .schedule_frame_in(crate::terminal_title::SPINNER_INTERVAL);
        }
    }

    /// Clear the managed terminal title and reset the cache.
    pub(crate) fn clear_managed_terminal_title(&mut self) -> std::io::Result<()> {
        self.last_terminal_title = None;
        crate::terminal_title::clear_terminal_title()
    }

    /// Called before every frame draw to advance the terminal title spinner.
    pub(crate) fn pre_draw_tick(&mut self) {
        if self.should_animate_terminal_title_spinner() {
            self.refresh_terminal_title();
        }
        self.subagent_pre_draw_tick();
        self.status_note_tick();
    }

    /// Whether the user cancelled and the agent ignored it, so that Esc now stops the agent.
    pub(crate) fn cancel_ignored(&self) -> bool {
        self.bottom_pane.status_note() == Some(&crate::bottom_pane::StatusNote::CancelIgnored)
    }

    /// Whether a first Esc has said that a second one stops the running subagents.
    pub(crate) fn stop_subagents_armed(&self) -> bool {
        matches!(
            self.bottom_pane.status_note(),
            Some(crate::bottom_pane::StatusNote::StopSubagents { until, .. })
                if Instant::now() < *until
        )
    }

    /// Say that Esc again, within two seconds, stops the running subagents.
    pub(crate) fn arm_stop_subagents(&mut self) {
        self.set_status_note(Some(crate::bottom_pane::StatusNote::StopSubagents {
            count: self.running_subagents(),
            until: Instant::now() + std::time::Duration::from_secs(2),
        }));
    }

    /// Say something in the status line for a while.
    pub(crate) fn set_status_note(&mut self, note: Option<crate::bottom_pane::StatusNote>) {
        self.bottom_pane.set_status_note(note);
        self.request_redraw();
    }

    /// Drop a note that has run out, and ask for the frame at which a countdown next changes.
    fn status_note_tick(&mut self) {
        let now = Instant::now();
        let Some(note) = self.bottom_pane.status_note() else {
            return;
        };
        if note.is_over(now) {
            self.bottom_pane.set_status_note(None);
        } else if let Some(at) = note.next_change(now) {
            self.frame_requester
                .schedule_frame_in(at.saturating_duration_since(now));
        }
    }
}

struct TrackedArea<'a, R> {
    inner: R,
    area: &'a std::cell::Cell<Option<ratatui::layout::Rect>>,
}

impl<R: Renderable> Renderable for TrackedArea<'_, R> {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        self.area.set(Some(area));
        self.inner.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.inner.desired_height(width)
    }

    fn cursor_pos(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        self.inner.cursor_pos(area)
    }
}

struct TrackedActiveTool<'a> {
    inner: &'a layout_cache::CachedCell,
    call_id: String,
    hits: &'a std::cell::RefCell<Vec<(ratatui::layout::Rect, String)>>,
}

impl Renderable for TrackedActiveTool<'_> {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        self.hits.borrow_mut().push((area, self.call_id.clone()));
        self.inner.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.inner.desired_height(width)
    }

    fn cursor_pos(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        self.inner.cursor_pos(area)
    }
}
