//! Turning ACP session updates into [`ClientEvent`]s: merging tool-call updates into snapshots,
//! reading invocations out of the shapes different agents use, and picking out the Nori status
//! messages that ride on session info.

use std::collections::HashMap;

use nori_protocol::acp::MaybeUndefined;
use nori_protocol::acp::v1 as acp;

use super::*;

mod tool_calls;

use tool_calls::*;

#[derive(Debug, Default)]
pub struct ClientEventNormalizer {
    tool_calls: HashMap<String, acp::ToolCall>,
}

impl ClientEventNormalizer {
    pub fn push_session_update(&mut self, update: &acp::SessionUpdate) -> Vec<ClientEvent> {
        match update {
            acp::SessionUpdate::UserMessageChunk(chunk) => {
                message_delta_from_chunk(chunk, MessageStream::User)
                    .into_iter()
                    .map(ClientEvent::MessageDelta)
                    .collect()
            }
            acp::SessionUpdate::AgentMessageChunk(chunk) => {
                message_delta_from_chunk(chunk, MessageStream::Answer)
                    .into_iter()
                    .map(ClientEvent::MessageDelta)
                    .collect()
            }
            acp::SessionUpdate::AgentThoughtChunk(chunk) => {
                message_delta_from_chunk(chunk, MessageStream::Reasoning)
                    .into_iter()
                    .map(ClientEvent::MessageDelta)
                    .collect()
            }
            acp::SessionUpdate::Plan(plan) => {
                vec![ClientEvent::PlanSnapshot(plan_snapshot_from_acp(plan))]
            }
            acp::SessionUpdate::ToolCall(tool_call) => {
                let call_id = tool_call.tool_call_id.to_string();
                self.tool_calls.insert(call_id, tool_call.clone());

                if is_generic_tool_call(tool_call) {
                    return Vec::new();
                }

                vec![ClientEvent::ToolSnapshot(tool_snapshot_from_tool_call(
                    tool_call,
                    ToolPhase::from_status(tool_call.status),
                ))]
            }
            acp::SessionUpdate::ToolCallUpdate(update) => {
                let call_id = update.tool_call_id.to_string();
                let entry = self.tool_calls.entry(call_id).or_insert_with(|| {
                    acp::ToolCall::new(update.tool_call_id.clone(), String::new())
                });
                entry.update(update.fields.clone());

                let phase = update
                    .fields
                    .status
                    .map(ToolPhase::from_status)
                    .unwrap_or_else(|| ToolPhase::from_status(entry.status));

                vec![ClientEvent::ToolSnapshot(tool_snapshot_from_tool_call(
                    entry, phase,
                ))]
            }
            acp::SessionUpdate::AvailableCommandsUpdate(update) => {
                let commands = update
                    .available_commands
                    .iter()
                    .map(|cmd| {
                        let input_hint = cmd.input.as_ref().map(|input| match input {
                            acp::AvailableCommandInput::Unstructured(u) => u.hint.clone(),
                            _ => String::new(),
                        });
                        AgentCommandInfo {
                            name: cmd.name.clone(),
                            description: cmd.description.clone(),
                            input_hint,
                        }
                    })
                    .collect();
                vec![ClientEvent::AgentCommandsUpdate(AgentCommandsUpdate {
                    commands,
                })]
            }
            acp::SessionUpdate::CurrentModeUpdate(update) => {
                vec![ClientEvent::SessionModeChanged(SessionModeChanged {
                    current_mode_id: update.current_mode_id.to_string(),
                })]
            }
            acp::SessionUpdate::ConfigOptionUpdate(update) => {
                vec![ClientEvent::SessionConfigUpdate(SessionConfigUpdate {
                    config_options: update.config_options.clone(),
                })]
            }
            acp::SessionUpdate::SessionInfoUpdate(update) => {
                if let Some(message) = nori_connection_message(update) {
                    vec![ClientEvent::SessionUpdateInfo(SessionUpdateInfo {
                        kind: SessionUpdateKind::SessionInfo,
                        message: message.to_string(),
                        hint: None,
                        usage: None,
                        session_info_patch: None,
                    })]
                // Hide only exact Nori lifecycle frames; preserve all displayable metadata.
                } else if is_nori_turn_status_only(update) {
                    Vec::new()
                } else {
                    vec![ClientEvent::SessionUpdateInfo(
                        session_update_info_from_session_info(update),
                    )]
                }
            }
            acp::SessionUpdate::UsageUpdate(update) => vec![ClientEvent::SessionUpdateInfo(
                session_update_info_from_usage(update),
            )],
            _ => Vec::new(),
        }
    }

    pub fn push_permission_request(
        &mut self,
        request: &acp::RequestPermissionRequest,
    ) -> Vec<ClientEvent> {
        let call_id = request.tool_call.tool_call_id.to_string();
        let entry = self.tool_calls.entry(call_id.clone()).or_insert_with(|| {
            acp::ToolCall::new(request.tool_call.tool_call_id.clone(), String::new())
        });
        entry.update(request.tool_call.fields.clone());

        let snapshot = tool_snapshot_from_tool_call(entry, ToolPhase::PendingApproval);
        let approval = ApprovalRequest {
            call_id,
            title: snapshot.title.clone(),
            kind: snapshot.kind.clone(),
            options: request
                .options
                .iter()
                .map(ApprovalOption::from_acp)
                .collect(),
            subject: ApprovalSubject::ToolSnapshot(snapshot),
        };

        vec![ClientEvent::ApprovalRequest(approval)]
    }
}

impl ToolPhase {
    fn from_status(status: acp::ToolCallStatus) -> Self {
        match status {
            acp::ToolCallStatus::Pending => ToolPhase::Pending,
            acp::ToolCallStatus::InProgress => ToolPhase::InProgress,
            acp::ToolCallStatus::Completed => ToolPhase::Completed,
            acp::ToolCallStatus::Failed => ToolPhase::Failed,
            _ => ToolPhase::Pending,
        }
    }
}

impl ToolKind {
    fn from_acp(kind: acp::ToolKind) -> Self {
        match kind {
            acp::ToolKind::Read => ToolKind::Read,
            acp::ToolKind::Search => ToolKind::Search,
            acp::ToolKind::Execute => ToolKind::Execute,
            acp::ToolKind::Edit => ToolKind::Edit,
            acp::ToolKind::Delete => ToolKind::Delete,
            acp::ToolKind::Move => ToolKind::Move,
            acp::ToolKind::Fetch => ToolKind::Fetch,
            acp::ToolKind::Think => ToolKind::Think,
            other => ToolKind::Other(format!("{other:?}")),
        }
    }
}

impl ApprovalOption {
    fn from_acp(option: &acp::PermissionOption) -> Self {
        Self {
            option_id: option.option_id.to_string(),
            name: option.name.clone(),
            kind: match option.kind {
                acp::PermissionOptionKind::AllowAlways => ApprovalOptionKind::AllowAlways,
                acp::PermissionOptionKind::AllowOnce => ApprovalOptionKind::AllowOnce,
                acp::PermissionOptionKind::RejectOnce => ApprovalOptionKind::RejectOnce,
                other => ApprovalOptionKind::Other(format!("{other:?}")),
            },
        }
    }
}

impl PlanStatus {
    fn from_acp(status: acp::PlanEntryStatus) -> Self {
        match status {
            acp::PlanEntryStatus::Pending => PlanStatus::Pending,
            acp::PlanEntryStatus::InProgress => PlanStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanStatus::Completed,
            _ => PlanStatus::Pending,
        }
    }
}

fn message_delta_from_chunk(
    chunk: &acp::ContentChunk,
    stream: MessageStream,
) -> Option<MessageDelta> {
    let delta = match (&chunk.content, stream) {
        (acp::ContentBlock::Text(text), _) if !text.text.is_empty() => text.text.clone(),
        (acp::ContentBlock::Image(_), MessageStream::User) => " [image attachment]".to_string(),
        (acp::ContentBlock::Audio(_), MessageStream::User) => " [audio attachment]".to_string(),
        (acp::ContentBlock::ResourceLink(resource), MessageStream::User) => {
            format!(" [resource: {}]", resource.name)
        }
        (acp::ContentBlock::Resource(_), MessageStream::User) => " [embedded resource]".to_string(),
        // An agent's message is markdown, so a markdown resource is part of it; other text is
        // shown as it is, in a block of its own.
        (acp::ContentBlock::Resource(resource), MessageStream::Answer) => {
            match &resource.resource {
                acp::EmbeddedResourceResource::TextResourceContents(contents)
                    if !contents.text.is_empty() =>
                {
                    match TextFormat::of_resource(contents.mime_type.as_deref(), &contents.uri) {
                        TextFormat::Markdown => format!("\n\n{}\n\n", contents.text.trim()),
                        TextFormat::Plain => {
                            format!("\n\n```\n{}\n```\n\n", contents.text.trim_end())
                        }
                    }
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(MessageDelta {
        stream,
        message_id: chunk.message_id.as_ref().map(ToString::to_string),
        delta,
    })
}

fn session_update_info_from_session_info(update: &acp::SessionInfoUpdate) -> SessionUpdateInfo {
    let mut parts = Vec::new();

    if let Some(title) = format_maybe_undefined_field("title", &update.title) {
        parts.push(title);
    }
    if let Some(updated_at) = format_maybe_undefined_field("updated_at", &update.updated_at) {
        parts.push(updated_at);
    }

    let message = if parts.is_empty() {
        "Session info updated".to_string()
    } else {
        format!("Session info updated: {}", parts.join("; "))
    };

    SessionUpdateInfo {
        kind: SessionUpdateKind::SessionInfo,
        message,
        hint: None,
        usage: None,
        session_info_patch: Some(SessionInfoPatch {
            title: update.title.clone(),
            updated_at: update.updated_at.clone(),
            meta: update.meta.clone(),
        }),
    }
}

fn nori_connection_message(update: &acp::SessionInfoUpdate) -> Option<&'static str> {
    if !update.title.is_undefined() || !update.updated_at.is_undefined() {
        return None;
    }

    let meta = update.meta.as_ref()?;
    let nori = meta.get("nori")?.as_object()?;
    let connection = nori.get("connection")?.as_object()?;
    if meta.len() != 1 || nori.len() != 1 || connection.len() != 1 {
        return None;
    }

    match connection.get("status")?.as_str()? {
        "reconnecting" => Some("Cloud connection lost. Reconnecting…"),
        "connected" => Some("Cloud connection restored."),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoriTurnStatus {
    Working,
    Idle,
}

pub(crate) fn nori_turn_status(update: &acp::SessionInfoUpdate) -> Option<NoriTurnStatus> {
    match update.meta.as_ref()?.get("nori")?.get("status")?.as_str()? {
        "working" => Some(NoriTurnStatus::Working),
        "idle" => Some(NoriTurnStatus::Idle),
        _ => None,
    }
}

fn is_nori_turn_status_only(update: &acp::SessionInfoUpdate) -> bool {
    nori_turn_status(update).is_some()
        && update.title.is_undefined()
        && update.updated_at.is_undefined()
}

fn format_maybe_undefined_field(label: &str, value: &MaybeUndefined<String>) -> Option<String> {
    match value.as_opt_ref() {
        Some(Some(value)) => Some(match label {
            "updated_at" => format!("{label}={value}"),
            _ => format!("{label}=\"{value}\""),
        }),
        Some(None) => Some(format!("{label}=null")),
        None => None,
    }
}

fn session_update_info_from_usage(update: &acp::UsageUpdate) -> SessionUpdateInfo {
    let mut message = format!("Session usage: {} / {} tokens", update.used, update.size);
    if let Some(cost) = &update.cost {
        message.push_str(&format!(", cost {:.2} {}", cost.amount, cost.currency));
    }

    SessionUpdateInfo {
        kind: SessionUpdateKind::Usage,
        message,
        hint: None,
        usage: Some(session_runtime::SessionUsageState {
            used_tokens: update.used as i64,
            total_tokens: update.size as i64,
            cost_display: update
                .cost
                .as_ref()
                .map(|cost| format!("{:.2} {}", cost.amount, cost.currency)),
        }),
        session_info_patch: None,
    }
}

fn plan_snapshot_from_acp(plan: &acp::Plan) -> PlanSnapshot {
    PlanSnapshot {
        entries: plan
            .entries
            .iter()
            .map(|entry| PlanEntry {
                step: entry.content.clone(),
                status: PlanStatus::from_acp(entry.status.clone()),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests;
