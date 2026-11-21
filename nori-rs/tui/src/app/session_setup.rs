use super::*;

const AGENT_PREPARATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
#[derive(Debug)]
pub(super) struct AgentPreparationTimeout;

impl std::fmt::Display for AgentPreparationTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "timed out preparing agent after {}s",
            AGENT_PREPARATION_TIMEOUT.as_secs()
        )
    }
}

impl std::error::Error for AgentPreparationTimeout {}

pub(super) struct ManualModelHandoff {
    pub(super) recorder: std::sync::Arc<nori_harness::transcript::TranscriptRecorder>,
    pub(super) context: String,
    /// The journal head `context` was read at.
    pub(super) head: Option<i64>,
    pub(super) source_generation: crate::app_event::SessionGeneration,
    pub(super) effort: Option<String>,
}

async fn prepare_agent_with_timeout(
    spec: nori_harness::runtime::AgentPrepareSpec,
) -> anyhow::Result<nori_harness::runtime::PreparedAgent> {
    match tokio::time::timeout(
        AGENT_PREPARATION_TIMEOUT,
        nori_harness::runtime::prepare_agent(spec),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err(anyhow::Error::new(AgentPreparationTimeout)),
    }
}

pub(super) fn onboarding_resume_event(
    sessions: Vec<nori_protocol::acp::v1::SessionInfo>,
) -> Option<AppEvent> {
    sessions.into_iter().find_map(|session| {
        let is_onboarding = session
            .meta
            .as_ref()
            .and_then(|meta| meta.get("nori"))
            .and_then(|nori| nori.get("purpose"))
            .and_then(serde_json::Value::as_str)
            == Some("onboarding");
        is_onboarding.then(|| AppEvent::ResumeAcpSession {
            acp_session_id: session.session_id.to_string(),
            title: session.title.filter(|title| !title.is_empty()),
        })
    })
}

pub(super) fn automatic_resume_event(
    session_id: Option<&nori_protocol::acp::v1::SessionId>,
) -> Option<AppEvent> {
    session_id.map(|session_id| AppEvent::ResumeAcpSession {
        acp_session_id: session_id.to_string(),
        title: None,
    })
}

impl App {
    /// Prepare a live pre-session agent (spawn → initialize → optional
    /// `session/list`) and report the still-owned connection as
    /// [`AppEvent::AgentPrepared`]. Used by picker-first entry
    /// (`nori cloud`), the post-`/close` return to the picker, and /resume
    /// retries on a deferred widget. Bounded by a wall-clock timeout so a
    /// hung broker can never wedge the boot with no way forward.
    pub(crate) fn begin_agent_preparation(&mut self, intent: crate::app_event::AgentPrepareIntent) {
        self.begin_agent_preparation_with_context(intent, None);
    }

    pub(crate) fn begin_agent_preparation_with_context(
        &mut self,
        intent: crate::app_event::AgentPrepareIntent,
        initial_context: Option<String>,
    ) {
        if let Some(preparation) = &mut self.primary_agent_preparation {
            if matches!(&intent, crate::app_event::AgentPrepareIntent::ResumePicker)
                && !matches!(
                    &preparation.intent,
                    crate::app_event::AgentPrepareIntent::ResumePicker
                )
            {
                let display_name = nori_harness::get_agent_display_name(&self.config.active_agent);
                self.chat_widget
                    .add_info_message(format!("Listing {display_name} sessions…"), None);
            }
            preparation.intent = intent;
            return;
        }
        let generation = crate::chatwidget::agent::next_session_generation();
        let display_name = nori_harness::get_agent_display_name(&self.config.active_agent);
        if !matches!(&intent, crate::app_event::AgentPrepareIntent::Idle) {
            self.chat_widget
                .add_info_message(format!("Listing {display_name} sessions…"), None);
        }

        if let Some(agent) = self.prepared_agent.take() {
            tokio::spawn(agent.shutdown());
        }
        self.prepared_agent_initial_context = None;
        let spec = crate::chatwidget::agent::agent_prepare_spec(
            self.config.clone(),
            initial_context.clone(),
        );
        let tx = self.app_event_tx.clone();
        let event_intent = intent.clone();
        let task = tokio::spawn(async move {
            let agent = prepare_agent_with_timeout(spec).await;
            tx.send(AppEvent::AgentPrepared {
                generation,
                agent,
                intent: event_intent,
            });
        });
        self.primary_agent_preparation = Some(PrimaryAgentPreparation {
            generation,
            abort: task.abort_handle(),
            intent,
            initial_context,
        });
    }

    /// Consume the primary prepared connection only after refreshing mutable
    /// session-time policy. On an identity-sensitive mismatch, reap it and
    /// restart preparation while the caller's pending activation remains set.
    pub(crate) fn take_refreshed_prepared_agent(
        &mut self,
    ) -> Result<Option<nori_harness::runtime::PreparedAgent>, String> {
        let Some(mut agent) = self.prepared_agent.take() else {
            return Ok(None);
        };
        let initial_context = self.prepared_agent_initial_context.take();
        let spec = crate::chatwidget::agent::agent_prepare_spec(
            self.config.clone(),
            initial_context.clone(),
        );
        if let Err(error) = nori_harness::runtime::refresh_prepared_agent(&mut agent, spec) {
            tokio::spawn(agent.shutdown());
            self.begin_agent_preparation_with_context(
                crate::app_event::AgentPrepareIntent::Idle,
                initial_context,
            );
            return Err(format!("prepared agent configuration changed: {error}"));
        }
        Ok(Some(agent))
    }

    /// Move the conversation to `model` on another agent. The conversation the new agent starts
    /// from is read in the background (it takes as long as the session is long, and nothing
    /// else on the event loop may wait for it); [`Self::continue_agent_model`] goes on with it.
    ///
    /// Chosen while a response is running, the switch is accepted at once and made when the
    /// response ends, so the response is not cut short; one line says so.
    pub(crate) fn select_agent_model(
        &mut self,
        agent_name: String,
        display_name: String,
        model: String,
    ) {
        let (Some(handle), None) = (
            self.chat_widget.harness_handle(),
            self.manual_model_handoff.as_ref(),
        ) else {
            self.start_agent_model_candidate(agent_name, display_name, model);
            return;
        };
        if self.chat_widget.is_task_running() {
            self.chat_widget.add_info_message(
                format!("Switching to {model} on {display_name} when this response ends."),
                None,
            );
            self.deferred_agent_model = Some(DeferredAgentModel {
                agent_name,
                display_name,
                model,
            });
            return;
        }
        let effort =
            self.chat_widget
                .agent_config()
                .effort_option()
                .and_then(|option| match &option.raw.kind {
                    nori_protocol::acp::v1::SessionConfigKind::Select(select) => {
                        Some(select.current_value.to_string())
                    }
                    _ => None,
                });
        self.model_handoff_token = self.model_handoff_token.saturating_add(1);
        let token = self.model_handoff_token;
        let source_generation = self.chat_widget.session_generation();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let context = async {
                let recorder = handle
                    .conversation_recorder()
                    .await
                    .map_err(|error| format!("Couldn't prepare model switch: {error}"))?
                    .ok_or_else(|| {
                        "Can't switch models without the conversation journal".to_owned()
                    })?;
                let (text, head) = recorder
                    .visible_context_at_head()
                    .await
                    .map_err(|error| format!("Couldn't read conversation: {error}"))?;
                Ok(crate::app_event::ModelHandoffContext {
                    recorder,
                    text,
                    head,
                })
            }
            .await;
            tx.send(AppEvent::ModelHandoffContext {
                token,
                agent_name,
                display_name,
                model,
                effort,
                source_generation,
                context,
            });
        });
    }

    /// Make the switch to another agent's model that was chosen during a response, once the
    /// response has ended. Called after every event; nothing to do is one check.
    pub(crate) fn run_deferred_agent_model(&mut self) {
        if self.chat_widget.is_task_running() {
            return;
        }
        if let Some(DeferredAgentModel {
            agent_name,
            display_name,
            model,
        }) = self.deferred_agent_model.take()
        {
            self.select_agent_model(agent_name, display_name, model);
        }
    }

    /// The conversation for a model switch to another agent is read: start that agent with it.
    pub(crate) fn continue_agent_model(
        &mut self,
        agent_name: String,
        display_name: String,
        model: String,
        effort: Option<String>,
        source_generation: crate::app_event::SessionGeneration,
        context: crate::app_event::ModelHandoffContext,
    ) {
        self.manual_model_handoff = Some(ManualModelHandoff {
            recorder: context.recorder,
            context: context.text,
            head: context.head,
            source_generation,
            effort,
        });
        self.start_agent_model_candidate(agent_name, display_name, model);
    }

    fn start_agent_model_candidate(
        &mut self,
        agent_name: String,
        display_name: String,
        model: String,
    ) {
        self.discard_candidate();
        crate::latency::begin(crate::latency::Interaction::AgentSwitch, false);
        self.pending_candidate_model = Some((agent_name.clone(), model));
        self.start_candidate_preparation(agent_name, display_name);
    }

    fn start_candidate_preparation(&mut self, agent_name: String, display_name: String) {
        let generation = crate::chatwidget::agent::next_session_generation();
        let initial_context = self
            .manual_model_handoff
            .as_ref()
            .map(|handoff| handoff.context.clone());
        let mut spec = crate::chatwidget::agent::agent_prepare_spec(
            self.config_for_agent(&agent_name),
            initial_context,
        );
        spec.handoff = self
            .manual_model_handoff
            .as_ref()
            .map(|handoff| handoff.recorder.clone());
        let tx = self.app_event_tx.clone();
        let prepared_agent_name = agent_name.clone();
        let task = tokio::spawn(async move {
            let agent = prepare_agent_with_timeout(spec).await;
            tx.send(AppEvent::AgentPrepared {
                generation,
                agent,
                intent: crate::app_event::AgentPrepareIntent::Candidate {
                    agent_name,
                    display_name,
                },
            });
        });
        self.candidate_agent = Some(CandidateAgent::Preparing {
            generation,
            agent_name: prepared_agent_name,
            abort: task.abort_handle(),
        });
    }

    /// What follows a candidate that is ready and chosen: resume the session the agent
    /// offers on its own, start a fresh one when there is nothing to resume, or ask.
    pub(crate) fn follow_up_prepared_candidate(&mut self) {
        let Some(CandidateAgent::Prepared { agent, .. }) = &self.candidate_agent else {
            return;
        };
        let automatic_resume =
            super::session_setup::automatic_resume_event(agent.automatic_session_id());
        let sessions = match agent.catalog() {
            nori_harness::runtime::SessionCatalog::Unsupported => Vec::new(),
            nori_harness::runtime::SessionCatalog::Listed(sessions) => sessions.clone(),
        };
        if let Some(event) = automatic_resume {
            self.app_event_tx.send(event);
        } else if sessions.is_empty() {
            // Nothing to resume: the agent just picked starts a session at once instead of
            // asking about a list with one row.
            self.app_event_tx.send(AppEvent::NewSession);
        } else {
            self.chat_widget
                .show_acp_resume_session_picker(sessions, true);
        }
    }

    pub(crate) fn cancel_primary_agent_preparation(&mut self) {
        if let Some(preparation) = self.primary_agent_preparation.take() {
            preparation.abort.abort();
        }
    }

    pub(crate) fn discard_candidate(&mut self) {
        // Ignore a conversation read that completes after this handoff was cancelled or
        // superseded by another model choice.
        self.model_handoff_token = self.model_handoff_token.saturating_add(1);
        crate::latency::abandon(crate::latency::Interaction::AgentSwitch);
        self.pending_candidate_model = None;
        self.drop_candidate();
        self.chat_widget.set_login_agent_override(None);
    }

    /// Tear down the candidate, leaving the chat widget's login override as it is.
    fn drop_candidate(&mut self) {
        match self.candidate_agent.take() {
            Some(CandidateAgent::Preparing { abort, .. }) => abort.abort(),
            Some(CandidateAgent::Prepared { agent, .. }) => {
                tokio::spawn((*agent).shutdown());
            }
            Some(CandidateAgent::Activating { widget, .. }) => {
                widget.shutdown_harness_session();
            }
            None => {}
        }
    }

    /// Replace the current widget with a sessionless one while a new child is
    /// prepared for an already-selected resume directive.
    pub(super) fn defer_resume_activation(
        &mut self,
        frame_requester: crate::tui::FrameRequester,
        acp_session_id: Option<String>,
        title: Option<String>,
        transcript: Option<nori_harness::transcript::Transcript>,
    ) {
        let (initial_prompt, initial_images) = self.chat_widget.take_initial_input();
        self.shutdown_current_conversation();
        let init = self.chat_widget_init(
            frame_requester,
            initial_prompt,
            initial_images,
            None,
            true,
            None,
        );
        self.chat_widget = ChatWidget::new(init);
        self.configure_new_chat_widget();
        self.pending_session_activation = Some(PendingSessionActivation::Resume {
            acp_session_id,
            title,
            transcript: transcript.map(Box::new),
        });
        self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
    }

    pub(super) fn config_for_agent(&self, agent_name: &str) -> NoriConfig {
        let mut config = self.config.clone();
        config.active_agent = agent_name.to_string();
        config.agent = agent_name.to_string();
        config
    }

    pub(crate) fn shutdown_current_conversation(&mut self) {
        self.chat_widget.shutdown_harness_session();
    }

    /// Display a loaded transcript in the history view.
    pub(super) fn display_viewonly_transcript(
        &mut self,
        entries: Vec<crate::viewonly_transcript::ViewonlyEntry>,
    ) {
        use crate::history_cell::AgentMarkdownCell;
        use crate::history_cell::AgentMessageCell;
        use crate::markdown::append_markdown;
        use crate::viewonly_transcript::ViewonlyEntry;

        // Add a header
        self.chat_widget.add_info_message(
            "────────── Viewing Previous Session ──────────".to_string(),
            None,
        );

        let mut is_first_entry = true;
        for entry in entries {
            // Add a blank line separator between entries (except before the first)
            if !is_first_entry {
                self.chat_widget
                    .add_plain_history_lines(vec![Line::from("")]);
            }
            is_first_entry = false;

            match entry {
                ViewonlyEntry::User { content } => {
                    // Add user messages with a user prefix to distinguish them
                    self.chat_widget.add_boxed_history(Box::new(
                        crate::history_cell::UserHistoryCell { message: content },
                    ));
                }
                ViewonlyEntry::Assistant { content } => {
                    let cell = AgentMarkdownCell::new(content, &self.config.cwd);
                    self.chat_widget.add_boxed_history(Box::new(cell));
                }
                ViewonlyEntry::Thinking { content } => {
                    // Add thinking block with dimmed style (same pattern as reasoning display)
                    let mut lines = Vec::new();
                    append_markdown(&content, None, &mut lines);
                    // Dim all spans in the lines to indicate this is thinking content
                    let dimmed_lines: Vec<Line<'static>> = lines
                        .into_iter()
                        .map(|line| {
                            Line::from(
                                line.spans
                                    .into_iter()
                                    .map(ratatui::prelude::Stylize::dim)
                                    .collect::<Vec<_>>(),
                            )
                        })
                        .collect();
                    let cell = AgentMessageCell::new(dimmed_lines, true);
                    self.chat_widget.add_boxed_history(Box::new(cell));
                }
                ViewonlyEntry::Info { content } => {
                    // Add as an info message
                    self.chat_widget
                        .add_info_message(content, Some("transcript".to_string()));
                }
            }
        }

        self.chat_widget
            .add_info_message("────────── End of Transcript ──────────".to_string(), None);
    }

    pub(super) fn open_external_editor(&mut self, tui: &mut tui::Tui) {
        use crate::editor;

        let current_text = self.chat_widget.composer_text();
        let editor_cmd = editor::resolve_editor();

        let temp_path = match editor::write_temp_file(&current_text) {
            Ok(path) => path,
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to create temp file: {err}"));
                return;
            }
        };

        // Restore terminal to normal mode so the editor can take over
        let _ = tui::restore();

        let status = editor::spawn_editor(&editor_cmd, &temp_path);

        // Re-enable TUI mode
        let _ = tui::set_modes();
        tui.frame_requester().schedule_frame();

        match status {
            Ok(exit_status) if exit_status.success() => {
                match editor::read_and_cleanup_temp_file(&temp_path) {
                    Ok(content) => {
                        let trimmed = content.trim_end().to_string();
                        self.chat_widget.set_composer_text(trimmed);
                    }
                    Err(err) => {
                        self.chat_widget
                            .add_error_message(format!("Failed to read editor output: {err}"));
                    }
                }
            }
            Ok(_) => {
                // Editor exited with non-zero status; discard changes, clean up temp file
                let _ = std::fs::remove_file(&temp_path);
            }
            Err(err) => {
                let _ = std::fs::remove_file(&temp_path);
                self.chat_widget
                    .add_error_message(format!("Failed to launch editor '{editor_cmd}': {err}"));
            }
        }
    }

    /// Launch a terminal file manager in chooser mode, then open the selected
    /// file in the user's editor.
    pub(super) fn browse_files(&mut self, fm: nori_config::FileManager, tui: &mut tui::Tui) {
        use crate::editor;

        // Create a temp file for the file manager to write the chosen path into.
        let chooser_output = match tempfile::Builder::new()
            .prefix("nori-browse-")
            .suffix(".txt")
            .tempfile()
        {
            Ok(tmp) => match tmp.keep() {
                Ok((_, path)) => path,
                Err(e) => {
                    self.chat_widget
                        .add_error_message(format!("Failed to create temp file: {}", e.error));
                    return;
                }
            },
            Err(err) => {
                self.chat_widget
                    .add_error_message(format!("Failed to create temp file: {err}"));
                return;
            }
        };

        let chooser_args = fm.chooser_args(&chooser_output);

        // Restore terminal to normal mode so the file manager can take over.
        let _ = tui::restore();

        // Loop: launch file manager → open selected file in editor → re-launch
        // file manager. The user stays in the browse workflow until they exit
        // the file manager without selecting a file (or it fails).
        loop {
            // Clear the chooser output so a stale selection from a previous
            // iteration doesn't persist.
            let _ = std::fs::write(&chooser_output, "");

            let fm_status = std::process::Command::new(fm.command_name())
                .args(&chooser_args)
                .stdin(std::process::Stdio::inherit())
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .status();

            match fm_status {
                Ok(exit_status) if exit_status.success() => {
                    let chosen = std::fs::read_to_string(&chooser_output)
                        .unwrap_or_default()
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .to_string();

                    if chosen.is_empty() {
                        // User exited file manager without selecting a file.
                        break;
                    }

                    let chosen_path = std::path::Path::new(&chosen);
                    if chosen_path.is_file() {
                        let editor_cmd = editor::resolve_editor();
                        let editor_status = editor::spawn_editor(&editor_cmd, chosen_path);
                        if let Err(err) = editor_status {
                            self.chat_widget.add_error_message(format!(
                                "Failed to launch editor '{editor_cmd}': {err}"
                            ));
                            break;
                        }
                        // After editor exits, loop back to re-launch the file manager.
                    } else {
                        self.chat_widget
                            .add_error_message(format!("Selected path is not a file: {chosen}"));
                        break;
                    }
                }
                Ok(_) => {
                    // File manager exited with non-zero status.
                    break;
                }
                Err(err) => {
                    self.chat_widget.add_error_message(format!(
                        "Failed to launch {}: {err}. Is it installed?",
                        fm.command_name()
                    ));
                    break;
                }
            }
        }

        // Always clean up temp file and restore TUI.
        let _ = std::fs::remove_file(&chooser_output);
        let _ = tui::set_modes();
        tui.frame_requester().schedule_frame();
    }

    #[cfg(target_os = "windows")]
    pub(super) fn spawn_world_writable_scan(
        cwd: PathBuf,
        env_map: std::collections::HashMap<String, String>,
        logs_base_dir: PathBuf,
        sandbox_policy: nori_config::SandboxPolicy,
        tx: AppEventSender,
    ) {
        tokio::task::spawn_blocking(move || {
            let result = codex_windows_sandbox::apply_world_writable_scan_and_denies(
                &logs_base_dir,
                &cwd,
                &env_map,
                &sandbox_policy,
                Some(logs_base_dir.as_path()),
            );
            if result.is_err() {
                // Scan failed: warn without examples.
                tx.send(AppEvent::OpenWorldWritableWarningConfirmation {
                    preset: None,
                    sample_paths: Vec::new(),
                    extra_count: 0usize,
                    failed_scan: true,
                });
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use nori_protocol::acp::v1::SessionInfo;

    use super::automatic_resume_event;
    use super::onboarding_resume_event;
    use crate::app_event::AppEvent;

    #[test]
    fn tagged_onboarding_session_emits_the_existing_resume_action() {
        let ordinary = SessionInfo::new("ordinary", PathBuf::from("/"));
        let mut onboarding = SessionInfo::new("onboarding", PathBuf::from("/"));
        onboarding.title = Some("Set up Nori".to_string());
        onboarding.meta = Some(
            serde_json::from_value(serde_json::json!({
                "nori": { "purpose": "onboarding" }
            }))
            .expect("valid metadata"),
        );

        assert!(matches!(
            onboarding_resume_event(vec![ordinary, onboarding]),
            Some(AppEvent::ResumeAcpSession { acp_session_id, title })
                if acp_session_id == "onboarding" && title.as_deref() == Some("Set up Nori")
        ));
    }

    #[test]
    fn no_tagged_onboarding_session_leaves_the_fresh_fallback_available() {
        let ordinary = SessionInfo::new("ordinary", PathBuf::from("/"));

        assert!(onboarding_resume_event(vec![ordinary]).is_none());
    }

    #[test]
    fn automatic_remote_control_session_bypasses_selection_with_resume_action() {
        let session_id = nori_protocol::acp::v1::SessionId::new("remote-active");

        assert!(matches!(
            automatic_resume_event(Some(&session_id)),
            Some(AppEvent::ResumeAcpSession { acp_session_id, title })
                if acp_session_id == "remote-active" && title.is_none()
        ));
        assert!(automatic_resume_event(None).is_none());
    }
}

/// A model on another agent, chosen while a response was running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeferredAgentModel {
    pub(crate) agent_name: String,
    pub(crate) display_name: String,
    pub(crate) model: String,
}
