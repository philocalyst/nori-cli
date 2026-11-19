//! Approval policy and the sandbox warnings around it.

use super::*;

impl App {
    pub(super) async fn handle_approvals_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::OpenFullAccessConfirmation { preset } => {
                self.chat_widget.open_full_access_confirmation(preset);
            }
            AppEvent::OpenWorldWritableWarningConfirmation {
                preset,
                sample_paths,
                extra_count,
                failed_scan,
            } => {
                self.chat_widget.open_world_writable_warning_confirmation(
                    preset,
                    sample_paths,
                    extra_count,
                    failed_scan,
                );
            }
            AppEvent::OpenWindowsSandboxEnablePrompt { preset } => {
                self.chat_widget.open_windows_sandbox_enable_prompt(preset);
            }
            AppEvent::EnableWindowsSandboxForAgentMode { preset } => {
                #[cfg(target_os = "windows")]
                {
                    match ConfigEditsBuilder::new(&self.config.nori_home)
                        .set_path(&["features", "enable_experimental_windows_sandbox"], true)
                        .apply()
                        .await
                    {
                        Ok(()) => {
                            codex_sandbox::set_windows_sandbox_enabled(true);
                            self.config.windows_sandbox_enabled = true;
                            self.config.forced_auto_mode_downgraded_on_windows = false;
                            self.chat_widget.clear_forced_auto_mode_downgrade();
                            if let Some((sample_paths, extra_count, failed_scan)) =
                                self.chat_widget.world_writable_warning_details()
                            {
                                self.app_event_tx.send(
                                    AppEvent::OpenWorldWritableWarningConfirmation {
                                        preset: Some(preset.clone()),
                                        sample_paths,
                                        extra_count,
                                        failed_scan,
                                    },
                                );
                            } else {
                                self.app_event_tx.send(AppEvent::ApplyApprovalPreset {
                                    approval: preset.approval,
                                    sandbox: preset.sandbox.clone(),
                                });
                                self.chat_widget.add_info_message(
                                    "Enabled experimental Windows sandbox.".to_string(),
                                    None,
                                );
                            }
                        }
                        Err(err) => {
                            tracing::error!(
                                error = %err,
                                "failed to enable Windows sandbox feature"
                            );
                            self.chat_widget.add_error_message(format!(
                                "Failed to enable the Windows sandbox feature: {err}"
                            ));
                        }
                    }
                }
                #[cfg(not(target_os = "windows"))]
                {
                    let _ = preset;
                }
            }
            AppEvent::ApplyApprovalPreset { approval, sandbox } => {
                #[cfg(target_os = "windows")]
                let sandbox_is_workspace_write_or_ro = matches!(
                    sandbox,
                    nori_config::SandboxPolicy::WorkspaceWrite { .. }
                        | nori_config::SandboxPolicy::ReadOnly
                );

                self.apply_approval_preset(approval, sandbox);

                // If sandbox policy becomes workspace-write or read-only, run the Windows world-writable scan.
                #[cfg(target_os = "windows")]
                {
                    // One-shot suppression if the user just confirmed continue.
                    if self.skip_world_writable_scan_once {
                        self.skip_world_writable_scan_once = false;
                        return Ok(true);
                    }

                    let should_check = codex_sandbox::get_platform_sandbox().is_some()
                        && sandbox_is_workspace_write_or_ro
                        && !self.chat_widget.world_writable_warning_hidden();
                    if should_check {
                        let cwd = self.config.cwd.clone();
                        let env_map: std::collections::HashMap<String, String> =
                            std::env::vars().collect();
                        let tx = self.app_event_tx.clone();
                        let logs_base_dir = self.config.nori_home.clone();
                        let sandbox_policy = self.config.sandbox_policy.clone();
                        Self::spawn_world_writable_scan(
                            cwd,
                            env_map,
                            logs_base_dir,
                            sandbox_policy,
                            tx,
                        );
                    }
                }
            }
            AppEvent::SkipNextWorldWritableScan => {
                self.skip_world_writable_scan_once = true;
            }
            AppEvent::UpdateFullAccessWarningAcknowledged(ack) => {
                self.chat_widget.set_full_access_warning_acknowledged(ack);
            }
            AppEvent::UpdateWorldWritableWarningAcknowledged(ack) => {
                self.chat_widget
                    .set_world_writable_warning_acknowledged(ack);
            }
            AppEvent::PersistFullAccessWarningAcknowledged => {
                if let Err(err) = ConfigEditsBuilder::new(&self.config.nori_home)
                    .set_path(&["notice", "hide_full_access_warning"], true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist full access warning acknowledgement"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save full access confirmation preference: {err}"
                    ));
                }
            }
            AppEvent::PersistWorldWritableWarningAcknowledged => {
                if let Err(err) = ConfigEditsBuilder::new(&self.config.nori_home)
                    .set_path(&["notice", "hide_world_writable_warning"], true)
                    .apply()
                    .await
                {
                    tracing::error!(
                        error = %err,
                        "failed to persist world-writable warning acknowledgement"
                    );
                    self.chat_widget.add_error_message(format!(
                        "Failed to save Agent mode warning preference: {err}"
                    ));
                }
            }
            AppEvent::OpenApprovalsPopup => {
                self.chat_widget.open_approvals_popup();
            }
            AppEvent::FullScreenApprovalRequest(request) => {
                let ApprovalRequest {
                    kind,
                    cwd,
                    snapshot,
                    ..
                } = request;
                let edit_changes = if matches!(
                    kind,
                    crate::presentation::ToolKind::Create
                        | crate::presentation::ToolKind::Edit
                        | crate::presentation::ToolKind::Delete
                        | crate::presentation::ToolKind::Move
                ) {
                    let mut changes =
                        client_tool_cell::diff_changes_from_artifacts(&snapshot.artifacts, &cwd);
                    if changes.is_empty() {
                        changes =
                            client_tool_cell::changes_from_invocation(&snapshot.invocation, &cwd);
                    }
                    if changes.is_empty() {
                        None
                    } else {
                        Some(changes)
                    }
                } else {
                    None
                };

                if let Some(changes) = edit_changes {
                    let diff_summary = DiffSummary::new(changes, cwd);
                    self.overlay = Some(Overlay::new_static_with_renderables(
                        vec![diff_summary.into()],
                        "P A T C H".to_string(),
                    ));
                } else {
                    let cell = client_tool_cell::ClientToolCell::new(*snapshot, cwd, false);
                    let lines = cell
                        .expanded_lines(tui.terminal.size().map(|size| size.width).unwrap_or(80));
                    self.overlay =
                        Some(Overlay::new_static_with_lines(lines, "T O O L".to_string()));
                }
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
