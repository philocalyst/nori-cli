//! Questions to the client: command approvals, patch approvals, permission requests and MCP
//! elicitations are sent as ACP permission requests, and their answers go back to Codex.

use std::collections::HashMap;

use crate::thread::ThreadMessage;
use crate::thread::client::SessionClient;
use crate::thread::mcp_approval::build_supported_mcp_elicitation_permission_request;
use crate::thread::patches::extract_tool_call_content_from_changes;
use crate::thread::prompt::PromptState;
use codex_protocol::approvals::ElicitationRequest;
use codex_protocol::approvals::ElicitationRequestEvent;
use codex_protocol::permissions::FileSystemAccessMode;
use codex_protocol::permissions::FileSystemPath;
use codex_protocol::permissions::FileSystemSandboxEntry;
use codex_protocol::permissions::FileSystemSpecialPath;
use codex_protocol::protocol::ApplyPatchApprovalRequestEvent;
use codex_protocol::protocol::ElicitationAction;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::request_permissions::PermissionGrantScope;
use codex_protocol::request_permissions::RequestPermissionProfile;
use codex_protocol::request_permissions::RequestPermissionsEvent;
use codex_protocol::request_permissions::RequestPermissionsResponse;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::PermissionOption;
use nori_acp_kit::schema::PermissionOptionKind;
use nori_acp_kit::schema::RequestPermissionOutcome;
use nori_acp_kit::schema::RequestPermissionResponse;
use nori_acp_kit::schema::SelectedPermissionOutcome;
use nori_acp_kit::schema::ToolCallId;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;
use tracing::info;
use tracing::warn;

pub(super) enum PendingPermissionRequest {
    Exec {
        approval_id: String,
        turn_id: String,
        option_map: HashMap<String, ReviewDecision>,
    },
    Patch {
        call_id: String,
        option_map: HashMap<String, ReviewDecision>,
    },
    RequestPermissions {
        call_id: String,
        permissions: RequestPermissionProfile,
    },
    McpElicitation {
        server_name: String,
        request_id: codex_protocol::mcp::RequestId,
        option_map: HashMap<String, ResolvedMcpElicitation>,
    },
}

pub(super) struct PendingPermissionInteraction {
    pub(super) id: u64,
    pub(super) request: PendingPermissionRequest,
}

#[derive(Clone)]
pub(super) struct ResolvedMcpElicitation {
    pub(super) action: ElicitationAction,
    pub(super) content: Option<serde_json::Value>,
    pub(super) meta: Option<serde_json::Value>,
}

impl ResolvedMcpElicitation {
    pub(super) fn accept() -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: None,
            meta: None,
        }
    }

    pub(super) fn accept_with_persist(persist: &'static str) -> Self {
        Self {
            action: ElicitationAction::Accept,
            content: None,
            meta: Some(serde_json::json!({ "persist": persist })),
        }
    }

    pub(super) fn cancel() -> Self {
        Self {
            action: ElicitationAction::Cancel,
            content: None,
            meta: None,
        }
    }
}

pub(super) fn exec_request_key(call_id: &str) -> String {
    format!("exec:{call_id}")
}

pub(super) fn patch_request_key(call_id: &str) -> String {
    format!("patch:{call_id}")
}

pub(super) fn permissions_request_key(call_id: &str) -> String {
    format!("permissions:{call_id}")
}

pub(super) fn mcp_elicitation_request_key(
    server_name: &str,
    request_id: &codex_protocol::mcp::RequestId,
) -> String {
    format!("mcp-elicitation:{server_name}:{request_id}")
}
impl PromptState {
    pub(super) fn spawn_permission_request(
        &mut self,
        client: &SessionClient,
        request_key: String,
        pending_request: PendingPermissionRequest,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
    ) {
        let interaction_id = self.next_permission_interaction_id;
        self.next_permission_interaction_id = self.next_permission_interaction_id.wrapping_add(1);
        let client = client.clone();
        let resolution_tx = self.resolution_tx.clone();
        let submission_id = self.submission_id.clone();
        let resolved_request_key = request_key.clone();
        drop(tokio::spawn(async move {
            let response = client.request_permission(tool_call, options).await;
            drop(
                resolution_tx.send(ThreadMessage::PermissionRequestResolved {
                    submission_id,
                    interaction_id,
                    request_key: resolved_request_key,
                    response,
                }),
            );
        }));

        self.pending_permission_interactions.insert(
            request_key,
            PendingPermissionInteraction {
                id: interaction_id,
                request: pending_request,
            },
        );
    }

    pub(super) async fn handle_permission_request_resolved(
        &mut self,
        _client: &SessionClient,
        interaction_id: u64,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    ) -> Result<(), Error> {
        let Some(pending_interaction_id) = self
            .pending_permission_interactions
            .get(&request_key)
            .map(|interaction| interaction.id)
        else {
            warn!("Ignoring permission response for unknown request key: {request_key}");
            return Ok(());
        };

        if pending_interaction_id != interaction_id {
            warn!("Ignoring stale permission response for request key: {request_key}");
            return Ok(());
        }

        let Some(interaction) = self.pending_permission_interactions.remove(&request_key) else {
            warn!("Ignoring permission response for unknown request key: {request_key}");
            return Ok(());
        };
        let pending_request = interaction.request;
        let response = response?;

        match pending_request {
            PendingPermissionRequest::Exec {
                approval_id,
                turn_id,
                option_map,
            } => {
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or(ReviewDecision::Abort),
                    RequestPermissionOutcome::Cancelled | _ => ReviewDecision::Abort,
                };

                self.thread
                    .submit(Op::ExecApproval {
                        id: approval_id,
                        turn_id: Some(turn_id),
                        decision,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::Patch {
                call_id,
                option_map,
            } => {
                let decision = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or(ReviewDecision::Abort),
                    RequestPermissionOutcome::Cancelled | _ => ReviewDecision::Abort,
                };

                self.thread
                    .submit(Op::PatchApproval {
                        id: call_id,
                        decision,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::RequestPermissions {
                call_id,
                permissions,
            } => {
                let response = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => match option_id.0.as_ref() {
                        "approved-for-session" => RequestPermissionsResponse {
                            permissions: permissions.clone(),
                            scope: PermissionGrantScope::Session,
                            strict_auto_review: false,
                        },
                        "approved" => RequestPermissionsResponse {
                            permissions: permissions.clone(),
                            scope: PermissionGrantScope::Turn,
                            strict_auto_review: false,
                        },
                        _ => RequestPermissionsResponse {
                            permissions: RequestPermissionProfile::default(),
                            scope: PermissionGrantScope::Turn,
                            strict_auto_review: true,
                        },
                    },
                    RequestPermissionOutcome::Cancelled | _ => RequestPermissionsResponse {
                        permissions: RequestPermissionProfile::default(),
                        scope: PermissionGrantScope::Turn,
                        strict_auto_review: true,
                    },
                };

                self.thread
                    .submit(Op::RequestPermissionsResponse {
                        id: call_id,
                        response,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
            PendingPermissionRequest::McpElicitation {
                server_name,
                request_id,
                option_map,
            } => {
                let response = match response.outcome {
                    RequestPermissionOutcome::Selected(SelectedPermissionOutcome {
                        option_id,
                        ..
                    }) => option_map
                        .get(option_id.0.as_ref())
                        .cloned()
                        .unwrap_or_else(ResolvedMcpElicitation::cancel),
                    RequestPermissionOutcome::Cancelled | _ => ResolvedMcpElicitation::cancel(),
                };

                self.thread
                    .submit(Op::ResolveElicitation {
                        server_name,
                        request_id,
                        decision: response.action,
                        content: response.content,
                        meta: response.meta,
                    })
                    .await
                    .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
            }
        }

        Ok(())
    }

    pub(super) async fn mcp_elicitation(
        &mut self,
        client: &SessionClient,
        event: ElicitationRequestEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let ElicitationRequestEvent {
            server_name,
            id,
            request,
            turn_id: _,
        } = event;
        if let Some(supported_request) = build_supported_mcp_elicitation_permission_request(
            &server_name,
            &id,
            &request,
            raw_input,
        ) {
            info!(
                "Routing MCP tool approval elicitation through ACP permission request: server={}, id={:?}",
                server_name, id
            );
            self.spawn_permission_request(
                client,
                supported_request.request_key,
                PendingPermissionRequest::McpElicitation {
                    server_name,
                    request_id: id,
                    option_map: supported_request.option_map,
                },
                supported_request.tool_call,
                supported_request.options,
            );
            return Ok(());
        }

        let request_kind = match &request {
            ElicitationRequest::Form { .. } => "form",
            ElicitationRequest::Url { .. } => "url",
            _ => "other",
        };

        info!(
            "Auto-declining unsupported MCP elicitation: server={}, id={:?}, kind={request_kind}",
            server_name, id
        );

        self.thread
            .submit(Op::ResolveElicitation {
                server_name,
                request_id: id,
                decision: ElicitationAction::Decline,
                content: None,
                meta: None,
            })
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;

        Ok(())
    }

    pub(super) fn patch_approval(
        &mut self,
        client: &SessionClient,
        event: ApplyPatchApprovalRequestEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let ApplyPatchApprovalRequestEvent {
            call_id,
            changes,
            reason,
            // grant_root doesn't seem to be set anywhere on the codex side
            ..
        } = event;
        let (title, locations, content) = extract_tool_call_content_from_changes(changes);
        let request_key = patch_request_key(&call_id);
        let options = vec![
            PermissionOption::new("approved", "Yes", PermissionOptionKind::AllowOnce),
            PermissionOption::new(
                "abort",
                "No, provide feedback",
                PermissionOptionKind::RejectOnce,
            ),
        ];
        self.spawn_permission_request(
            client,
            request_key,
            PendingPermissionRequest::Patch {
                call_id: call_id.clone(),
                option_map: HashMap::from([
                    ("approved".to_string(), ReviewDecision::Approved),
                    ("abort".to_string(), ReviewDecision::Abort),
                ]),
            },
            ToolCallUpdate::new(
                call_id,
                ToolCallUpdateFields::new()
                    .kind(ToolKind::Edit)
                    .status(ToolCallStatus::Pending)
                    .title(title)
                    .locations(locations)
                    .content(content.chain(reason.map(|r| r.into())).collect::<Vec<_>>())
                    .raw_input(raw_input),
            ),
            options,
        );
        Ok(())
    }

    pub(super) fn request_permissions(
        &mut self,
        client: &SessionClient,
        event: RequestPermissionsEvent,
    ) -> Result<(), Error> {
        let raw_input = serde_json::json!(&event);
        let RequestPermissionsEvent {
            call_id,
            turn_id: _,
            reason,
            permissions,
            ..
        } = event;

        // Create a new tool call for the command execution
        let tool_call_id = ToolCallId::new(call_id.clone());

        let mut content = vec![];

        if let Some(reason) = reason.as_ref() {
            content.push(reason.clone());
        }
        if let Some(file_system) = permissions.file_system.as_ref() {
            let reads = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Read),
            );
            if !reads.is_empty() {
                content.push(format!("File System Read Access: {reads}"));
            }
            let writes = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Write),
            );
            if !writes.is_empty() {
                content.push(format!("File System Write Access: {writes}"));
            }
            let denies = format_file_system_entries(
                file_system
                    .entries
                    .iter()
                    .filter(|entry| entry.access == FileSystemAccessMode::Deny),
            );
            if !denies.is_empty() {
                content.push(format!("File System Denied Access: {denies}"));
            }
        }
        if let Some(network) = permissions.network.as_ref()
            && let Some(enabled) = network.enabled
        {
            content.push(format!("Network Access: {enabled}"));
        }

        let content = if content.is_empty() {
            None
        } else {
            Some(vec![content.join("\n").into()])
        };

        self.spawn_permission_request(
            client,
            permissions_request_key(&call_id),
            PendingPermissionRequest::RequestPermissions {
                call_id,
                permissions,
            },
            ToolCallUpdate::new(
                tool_call_id,
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Pending)
                    .title(reason.unwrap_or_else(|| "Permissions Request".to_string()))
                    .raw_input(raw_input)
                    .content(content),
            ),
            vec![
                PermissionOption::new(
                    "approved-for-session",
                    "Yes, for session",
                    PermissionOptionKind::AllowAlways,
                ),
                PermissionOption::new("approved", "Yes", PermissionOptionKind::AllowOnce),
                PermissionOption::new("abort", "No", PermissionOptionKind::RejectOnce),
            ],
        );

        Ok(())
    }
}

pub(super) fn format_file_system_entries<'a>(
    entries: impl Iterator<Item = &'a FileSystemSandboxEntry>,
) -> String {
    entries
        .map(format_file_system_entry)
        .collect::<Vec<_>>()
        .join(", ")
}

pub(super) fn format_file_system_entry(entry: &FileSystemSandboxEntry) -> String {
    match &entry.path {
        FileSystemPath::Path { path } => path.to_path_buf().display().to_string(),
        FileSystemPath::GlobPattern { pattern } => format!("glob `{pattern}`"),
        FileSystemPath::Special { value } => format_file_system_special(value),
    }
}

pub(super) fn format_file_system_special(value: &FileSystemSpecialPath) -> String {
    match value {
        FileSystemSpecialPath::Root => ":root".to_string(),
        FileSystemSpecialPath::Minimal => ":minimal".to_string(),
        FileSystemSpecialPath::ProjectRoots { subpath } => {
            format_file_system_subpath(":project_roots", subpath.as_deref())
        }
        FileSystemSpecialPath::Tmpdir => ":tmpdir".to_string(),
        FileSystemSpecialPath::SlashTmp => "/tmp".to_string(),
        FileSystemSpecialPath::Unknown { path, subpath } => {
            format_file_system_subpath(path, subpath.as_deref())
        }
    }
}

pub(super) fn format_file_system_subpath(base: &str, subpath: Option<&str>) -> String {
    match subpath {
        Some(subpath) => format!("{base}/{subpath}"),
        None => base.to_string(),
    }
}
