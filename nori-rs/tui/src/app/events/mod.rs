use super::*;

mod agents;
mod approvals;
mod config;
mod extensions;
mod panels;
mod prepared;
mod resume;
mod session;
mod stream;
mod subagents;
mod transcript;
mod workspace;

#[derive(Debug, PartialEq, Eq)]
enum CandidateActivationEvent {
    Forward,
    CapturedError,
    Ended(String),
}

fn classify_candidate_activation_event(
    event: &nori_protocol::SessionEvent,
    activation_error: &mut Option<String>,
) -> CandidateActivationEvent {
    match event {
        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
            response: Err(error),
            ..
        }) => {
            tracing::warn!(
                code = i32::from(error.code),
                message = %error.message,
                detail = ChatWidget::acp_error_detail(error),
                "ACP candidate activation request failed"
            );
            if let Some(message) = ChatWidget::acp_error_message_with_detail(error) {
                *activation_error = Some(message);
            }
            CandidateActivationEvent::CapturedError
        }
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::RequestFailed(failure)) => {
            tracing::warn!(
                message = %failure.message,
                kind = ?failure.kind,
                "candidate activation request failed"
            );
            if activation_error.is_none() {
                *activation_error = Some(failure.message.clone());
            }
            CandidateActivationEvent::CapturedError
        }
        nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(ended)) => {
            CandidateActivationEvent::Ended(activation_error.take().unwrap_or_else(|| {
                ended
                    .message
                    .clone()
                    .unwrap_or_else(|| "Candidate session ended during activation".to_string())
            }))
        }
        _ => CandidateActivationEvent::Forward,
    }
}

fn candidate_model_config(
    options: &[nori_protocol::acp::v1::SessionConfigOption],
    model: &str,
) -> Option<String> {
    use nori_protocol::acp::v1 as acp;
    options.iter().find_map(|option| {
        if option.category != Some(acp::SessionConfigOptionCategory::Model) {
            return None;
        }
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let offered = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options
                .iter()
                .any(|option| option.value.to_string() == model),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| &group.options)
                .any(|option| option.value.to_string() == model),
            _ => false,
        };
        offered.then(|| option.id.to_string())
    })
}

impl App {
    /// Clear and report a due deferred agent spawn.
    ///
    /// Per-session startup defers the agent spawn until the chosen skillset's
    /// files are on disk. Returns `true` (clearing the pending flag) when a
    /// spawn is now due; `false` while agent preparation is still in
    /// flight or no spawn was deferred. Shared by the skillset-applied and
    /// picker-dismissed paths so their guard cannot drift.
    pub(super) fn take_deferred_spawn(&mut self) -> bool {
        if self.deferred_spawn_pending && self.primary_agent_preparation.is_none() {
            self.deferred_spawn_pending = false;
            true
        } else {
            false
        }
    }

    /// Resolve a deferred agent spawn and refresh system info after a skillset
    /// install or switch completes. Both `SkillsetInstallResult` and
    /// `SkillsetSwitchResult` route here: a per-session startup that lands on a
    /// non-worktree cwd installs (rather than switches), so the deferred spawn
    /// must resolve on either result or the agent never starts.
    fn on_skillset_applied(&mut self, success: bool) {
        if !success {
            return;
        }
        if self.take_deferred_spawn() {
            // The chosen skillset has written its workspace state, so activate
            // the session now instead of waiting for the first prompt.
            self.pending_session_activation = Some(PendingSessionActivation::New);
            self.begin_agent_preparation(crate::app_event::AgentPrepareIntent::Idle);
        }
        self.request_system_info_refresh(
            self.config.cwd.clone(),
            self.config.active_agent.clone().into(),
            self.chat_widget.first_prompt_text(),
        );
    }

    pub(super) fn apply_approval_preset(
        &mut self,
        approval: AskForApproval,
        sandbox: SandboxPolicy,
    ) {
        self.config.approval_policy = approval;
        self.config.sandbox_policy = sandbox.clone();
        #[cfg(target_os = "windows")]
        if !matches!(sandbox, nori_config::SandboxPolicy::ReadOnly)
            || codex_sandbox::get_platform_sandbox().is_some()
        {
            self.config.forced_auto_mode_downgraded_on_windows = false;
        }
        self.chat_widget.set_approval_policy(approval);
        self.chat_widget.set_sandbox_policy(sandbox);
    }

    pub(super) async fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        if self.waiting_model_presses.is_waiting()
            && self.session_view.focused() == crate::subagent_switcher::Target::Main
            && !self.chat_widget.agent_config().is_empty()
            && let Some((step, count)) = self.waiting_model_presses.take(std::time::Instant::now())
        {
            for _ in 0..count {
                self.chat_widget.cycle_model(step);
            }
        }
        match event {
            AppEvent::BranchFromCurrent
            | AppEvent::ChildHistoryActionFinished { .. }
            | AppEvent::ChildHistoryReady { .. }
            | AppEvent::ChildSessionOutput { .. }
            | AppEvent::ContextTimelineReady { .. }
            | AppEvent::ForkResponseSubagent { .. }
            | AppEvent::OpenActorPanel
            | AppEvent::SendSubagentMessage { .. }
            | AppEvent::SubagentConfigSetResult { .. }
            | AppEvent::SubagentControlsReady { .. }
            | AppEvent::SubagentMessageFailed { .. } => {
                self.handle_subagents_event(tui, event).await
            }
            AppEvent::BeginExit
            | AppEvent::ConfirmRemoteControlExplicit(..)
            | AppEvent::ExitRequest
            | AppEvent::NewSession
            | AppEvent::OpenAgentSessionPicker
            | AppEvent::RemoteControlRequested(..)
            | AppEvent::SessionCloseFailed { .. }
            | AppEvent::SessionClosed => self.handle_session_event(tui, event).await,
            AppEvent::EditorClipboard { command, text } => {
                self.chat_widget.paste_clipboard(command, &text);
                tui.frame_requester().schedule_frame();
                Ok(true)
            }
            AppEvent::ConsolidateAgentMessage { .. }
            | AppEvent::DiffResult(..)
            | AppEvent::FinalizeHistoryTools { .. }
            | AppEvent::FocusSnapshotsLoaded { .. }
            | AppEvent::ForkToMessage { .. }
            | AppEvent::HistoryReplay(..)
            | AppEvent::InsertHistoryCell(..)
            | AppEvent::ResponseBoundary(..)
            | AppEvent::ResponseRewindFinished { .. }
            | AppEvent::RevealUserQuestion
            | AppEvent::RewindFilesRestored { .. }
            | AppEvent::RewindSelection(..)
            | AppEvent::ForkWalkedTurn { .. }
            | AppEvent::ShellComplete { .. }
            | AppEvent::ShellCompleted { .. }
            | AppEvent::ShowToast(..)
            | AppEvent::TransientNotice(..)
            | AppEvent::UpdateHistoryToolSnapshot { .. } => {
                self.handle_transcript_event(tui, event).await
            }
            AppEvent::CommitChanges { .. }
            | AppEvent::CommitChangesReady { .. }
            | AppEvent::CustomPromptsLoaded(..)
            | AppEvent::FileSearchResult { .. }
            | AppEvent::GoalLoaded(..)
            | AppEvent::HarnessAction(..)
            | AppEvent::HarnessActionFailed(..)
            | AppEvent::HistoryEntryLoaded { .. }
            | AppEvent::HistorySearchLoaded(..)
            | AppEvent::PromptSubmitFailed(..)
            | AppEvent::RefreshSystemInfoForDirectory { .. }
            | AppEvent::StartFileSearch(..)
            | AppEvent::SystemInfoRefreshed(..)
            | AppEvent::UndoSnapshotsLoaded(..) => self.handle_workspace_event(tui, event).await,
            AppEvent::SessionEvent { .. } => self.handle_stream_event(tui, event).await,
            AppEvent::ActivatePreparedResume { .. } | AppEvent::AgentPrepared { .. } => {
                self.handle_prepared_event(tui, event).await
            }
            AppEvent::ApplyApprovalPreset { .. }
            | AppEvent::EnableWindowsSandboxForAgentMode { .. }
            | AppEvent::FullScreenApprovalRequest(..)
            | AppEvent::OpenApprovalsPopup
            | AppEvent::OpenFullAccessConfirmation { .. }
            | AppEvent::OpenWindowsSandboxEnablePrompt { .. }
            | AppEvent::OpenWorldWritableWarningConfirmation { .. }
            | AppEvent::PersistFullAccessWarningAcknowledged
            | AppEvent::PersistWorldWritableWarningAcknowledged
            | AppEvent::SkipNextWorldWritableScan
            | AppEvent::UpdateFullAccessWarningAcknowledged(..)
            | AppEvent::UpdateWorldWritableWarningAcknowledged(..) => {
                self.handle_approvals_event(tui, event).await
            }
            AppEvent::AcpModeConfigSnapshot { .. }
            | AppEvent::AcpSessionConfigSetResult { .. }
            | AppEvent::AcpSessionConfigSnapshot { .. }
            | AppEvent::AgentConnecting { .. }
            | AppEvent::AgentSpawnFailed { .. }
            | AppEvent::CancelAgentCandidate
            | AppEvent::CycleEffort(..)
            | AppEvent::CycleModel(..)
            | AppEvent::ExternalCliLoginComplete { .. }
            | AppEvent::ExternalCliLoginOutput { .. }
            | AppEvent::LoginCancelled
            | AppEvent::LoginComplete { .. }
            | AppEvent::LoginProvider { .. }
            | AppEvent::SignInMethods { .. }
            | AppEvent::SignInWith { .. }
            | AppEvent::SignInVerified { .. }
            | AppEvent::AddAccount { .. }
            | AppEvent::AccountAdded { .. }
            | AppEvent::LoginStarted { .. }
            | AppEvent::OpenAcpModelPickerUnsupported
            | AppEvent::OpenAcpSessionConfigPicker { .. }
            | AppEvent::OpenAcpSessionConfigValuePicker { .. }
            | AppEvent::OpenCustomModelInput { .. }
            | AppEvent::SelectModelRoutes { .. }
            | AppEvent::SetAcpSessionConfigOption { .. } => {
                self.handle_agents_event(tui, event).await
            }
            AppEvent::BrowseFiles(..)
            | AppEvent::BrowserLaunchFailed(..)
            | AppEvent::BrowserLaunched { .. }
            | AppEvent::OpenAutoWorktreePicker
            | AppEvent::OpenFileManagerPicker
            | AppEvent::OpenFooterSegmentsPicker
            | AppEvent::OpenHotkeyPicker
            | AppEvent::OpenLoopCountPicker
            | AppEvent::OpenNotifyAfterIdlePicker
            | AppEvent::OpenScriptTimeoutPicker
            | AppEvent::OpenSkillsetPerSessionWorktreeChoice
            | AppEvent::OpenVimModePicker
            | AppEvent::SetBrowserProfile(..)
            | AppEvent::SetConfigAcpWireRecording(..)
            | AppEvent::SetConfigAutoWorktree(..)
            | AppEvent::SetConfigCustomWorkingMessages(..)
            | AppEvent::SetConfigFileManager(..)
            | AppEvent::SetConfigFooterSegment(..)
            | AppEvent::SetConfigHotkey { .. }
            | AppEvent::SetConfigLoopCount(..)
            | AppEvent::SetConfigNotifyAfterIdle(..)
            | AppEvent::SetConfigOsNotifications(..)
            | AppEvent::SetConfigPinnedPlanDrawer(..)
            | AppEvent::SetConfigScriptTimeout(..)
            | AppEvent::SetConfigSkillsetPerSession(..)
            | AppEvent::SetConfigTerminalNotifications(..)
            | AppEvent::SetConfigVerticalFooter(..)
            | AppEvent::SetConfigVimMode { .. } => self.handle_config_event(tui, event).await,
            AppEvent::ComputeMcpAuthStatuses
            | AppEvent::ExecuteScript { .. }
            | AppEvent::InstallSkillset { .. }
            | AppEvent::LoopIteration { .. }
            | AppEvent::McpAuthStatusesReady(..)
            | AppEvent::McpOAuthLogin { .. }
            | AppEvent::McpOAuthLoginCancel { .. }
            | AppEvent::McpOAuthLoginComplete { .. }
            | AppEvent::SaveMcpServers(..)
            | AppEvent::ScriptExecutionComplete { .. }
            | AppEvent::SkillsetInstallResult { .. }
            | AppEvent::SkillsetListResult { .. }
            | AppEvent::SkillsetPickerDismissed
            | AppEvent::SkillsetSwitchResult { .. }
            | AppEvent::SwitchSkillset { .. } => self.handle_extensions_event(tui, event).await,
            AppEvent::DisplayViewonlyTranscript { .. }
            | AppEvent::InsertSessionReference { .. }
            | AppEvent::LoadSessionReference { .. }
            | AppEvent::LoadViewonlyTranscript { .. }
            | AppEvent::OpenForkPicker
            | AppEvent::OpenUsage
            | AppEvent::ShowChoicePicker(..)
            | AppEvent::ShowReferenceSessionPicker { .. }
            | AppEvent::ShowViewonlySessionPicker { .. } => {
                self.handle_panels_event(tui, event).await
            }
            AppEvent::ResumeAcpSession { .. }
            | AppEvent::ResumeSession { .. }
            | AppEvent::ResumeTranscriptLoaded { .. }
            | AppEvent::ShowHelixAcpResumePicker { .. }
            | AppEvent::ShowResumeSessionPicker { .. } => {
                self.handle_resume_event(tui, event).await
            }
        }
    }
}

/// Compose the info-cell message shown when resuming/reattaching to a session.
///
/// Cloud reattach wording names the selected broker session. It deliberately
/// makes no claim about replay: that depends on the facade's capabilities.
pub(super) fn reattach_info_message(
    acp_session_id: &str,
    title: Option<&str>,
    cloud_mode: bool,
    display_name: &str,
) -> String {
    if cloud_mode {
        let session_label = match title {
            Some(title) => format!("{acp_session_id} ({title})"),
            None => acp_session_id.to_string(),
        };
        format!("Reattaching to {session_label}...")
    } else {
        format!("Resuming session with {display_name}...")
    }
}

/// Take back the transcript of a resume left pending in case the agent was not ready, now that it
/// is: moving it out, rather than having kept a copy, holds one transcript in memory at a time.
fn take_pending_resume_transcript(
    pending: &mut Option<PendingSessionActivation>,
) -> Option<nori_harness::transcript::Transcript> {
    match pending.take() {
        Some(PendingSessionActivation::Resume { transcript, .. }) => {
            transcript.map(|transcript| *transcript)
        }
        Some(PendingSessionActivation::New | PendingSessionActivation::ResumeLoading) | None => {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CandidateActivationEvent;
    use super::classify_candidate_activation_event;
    use super::reattach_info_message;

    #[test]
    fn candidate_model_must_still_be_advertised_by_the_activated_agent() {
        use nori_protocol::acp::v1 as acp;
        use pretty_assertions::assert_eq;
        let option = acp::SessionConfigOption::select(
            "model",
            "Model",
            "first",
            vec![acp::SessionConfigSelectOption::new("first", "First")],
        )
        .category(acp::SessionConfigOptionCategory::Model);
        assert_eq!(
            super::candidate_model_config(std::slice::from_ref(&option), "first"),
            Some("model".to_string())
        );
        assert_eq!(super::candidate_model_config(&[option], "stale"), None);
    }

    #[test]
    fn cloud_reattach_info_message_snapshot() {
        insta::assert_snapshot!(
            "cloud_reattach_info_message",
            reattach_info_message("session-123", Some("Fix reconnect"), true, "Nori Cloud")
        );
    }

    #[test]
    fn candidate_activation_prefers_structured_acp_error_detail() {
        let request_id = nori_protocol::acp::v1::RequestId::Str("session-new".to_string());
        let error = nori_protocol::acp::v1::Error::new(-32010, "broker unreachable").data(
            serde_json::json!({
                "detail": "connection reset by broker",
                "trace_id": "internal-only"
            }),
        );
        let mut raw_error = None;

        let response = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
            request_id,
            response: Err(error),
        });
        assert_eq!(
            classify_candidate_activation_event(&response, &mut raw_error),
            CandidateActivationEvent::CapturedError
        );
        assert_eq!(
            raw_error.as_deref(),
            Some("broker unreachable: connection reset by broker")
        );

        let ended = nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(
            nori_protocol::SessionEnded {
                reason: nori_protocol::SessionEndReason::SpawnFailed,
                message: Some("Failed to create session".to_string()),
            },
        ));
        assert_eq!(
            classify_candidate_activation_event(&ended, &mut raw_error),
            CandidateActivationEvent::Ended(
                "broker unreachable: connection reset by broker".to_string()
            )
        );
        assert_eq!(raw_error, None);
    }

    #[test]
    fn candidate_activation_defers_request_failure_until_session_end() {
        let mut activation_error = None;
        let failure_message = "Connection timed out. The agent did not respond.";
        let request_failed = nori_protocol::SessionEvent::Nori(
            nori_protocol::NoriEvent::RequestFailed(nori_protocol::RequestFailure {
                request_id: None,
                message: failure_message.to_string(),
                kind: nori_protocol::RequestFailureKind::Retryable,
            }),
        );

        assert_eq!(
            classify_candidate_activation_event(&request_failed, &mut activation_error),
            CandidateActivationEvent::CapturedError
        );
        assert_eq!(activation_error.as_deref(), Some(failure_message));

        let ended = nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::SessionEnded(
            nori_protocol::SessionEnded {
                reason: nori_protocol::SessionEndReason::TimedOut,
                message: Some(failure_message.to_string()),
            },
        ));
        assert_eq!(
            classify_candidate_activation_event(&ended, &mut activation_error),
            CandidateActivationEvent::Ended(failure_message.to_string())
        );
        assert_eq!(activation_error, None);
    }
}
