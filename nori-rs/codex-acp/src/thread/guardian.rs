//! Guardian reviews of risky actions, shown as a tool call that finishes with the verdict.

use crate::thread::client::SessionClient;
use crate::thread::prompt::PromptState;
use codex_protocol::approvals::GuardianAssessmentAction;
use codex_protocol::approvals::GuardianCommandSource;
use codex_protocol::protocol::GuardianAssessmentEvent;
use codex_protocol::protocol::GuardianAssessmentStatus;
use nori_acp_kit::schema::Content;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;

impl PromptState {
    pub(super) fn guardian_assessment(
        &mut self,
        client: &SessionClient,
        event: GuardianAssessmentEvent,
    ) {
        let call_id = guardian_assessment_tool_call_id(&event.id);
        let status = guardian_assessment_tool_call_status(&event.status);
        let content = guardian_assessment_content(&event);
        let raw_event = serde_json::json!(&event);

        match event.status {
            GuardianAssessmentStatus::InProgress => {
                if self.active_guardian_assessments.insert(event.id.clone()) {
                    client.send_tool_call(
                        ToolCall::new(call_id, "Guardian Review")
                            .kind(ToolKind::Think)
                            .status(status)
                            .content(content)
                            .raw_input(raw_event),
                    );
                } else {
                    client.send_tool_call_update(ToolCallUpdate::new(
                        call_id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(content)
                            .raw_output(raw_event),
                    ));
                }
            }
            GuardianAssessmentStatus::TimedOut
            | GuardianAssessmentStatus::Approved
            | GuardianAssessmentStatus::Denied
            | GuardianAssessmentStatus::Aborted => {
                if self.active_guardian_assessments.remove(&event.id) {
                    client.send_tool_call_update(ToolCallUpdate::new(
                        call_id,
                        ToolCallUpdateFields::new()
                            .status(status)
                            .content(content)
                            .raw_output(raw_event),
                    ));
                } else {
                    client.send_tool_call(
                        ToolCall::new(call_id, "Guardian Review")
                            .kind(ToolKind::Think)
                            .status(status)
                            .content(content)
                            .raw_input(raw_event),
                    );
                }
            }
        }
    }
}

pub(super) fn guardian_assessment_tool_call_id(id: &str) -> String {
    format!("guardian_assessment:{id}")
}

pub(super) fn guardian_assessment_tool_call_status(
    status: &GuardianAssessmentStatus,
) -> ToolCallStatus {
    match status {
        GuardianAssessmentStatus::InProgress => ToolCallStatus::InProgress,
        GuardianAssessmentStatus::Approved => ToolCallStatus::Completed,
        GuardianAssessmentStatus::Denied
        | GuardianAssessmentStatus::Aborted
        | GuardianAssessmentStatus::TimedOut => ToolCallStatus::Failed,
    }
}

pub(super) fn guardian_assessment_content(event: &GuardianAssessmentEvent) -> Vec<ToolCallContent> {
    let mut lines = vec![format!(
        "Status: {}",
        match event.status {
            GuardianAssessmentStatus::InProgress => "In progress",
            GuardianAssessmentStatus::Approved => "Approved",
            GuardianAssessmentStatus::Denied => "Denied",
            GuardianAssessmentStatus::Aborted => "Aborted",
            GuardianAssessmentStatus::TimedOut => "Timed out",
        }
    )];

    if let Some(summary) = guardian_action_summary(&event.action) {
        lines.push(format!("Action: {summary}"));
    }

    if let Some(level) = event.risk_level {
        lines.push(format!("Risk: {}", format!("{level:?}").to_lowercase()));
    }

    if let Some(rationale) = event.rationale.as_ref()
        && !rationale.trim().is_empty()
    {
        lines.push(format!("Rationale: {rationale}"));
    }

    let content = vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
        TextContent::new(lines.join("\n")),
    )))];

    content
}

pub(super) fn guardian_action_summary(action: &GuardianAssessmentAction) -> Option<String> {
    match action {
        GuardianAssessmentAction::Command {
            source,
            command,
            cwd: _,
        } => {
            let label = guardian_command_source_label(source);
            Some(format!("{label} {command}"))
        }
        GuardianAssessmentAction::Execve {
            source,
            program,
            argv,
            cwd: _,
        } => {
            let label = guardian_command_source_label(source);
            let command: Vec<&str> = if argv.is_empty() {
                vec![program.as_str()]
            } else {
                argv.iter().map(String::as_str).collect()
            };
            let joined = shlex::try_join(command.iter().copied())
                .ok()
                .unwrap_or_else(|| command.join(" "));
            Some(format!("{label} {joined}"))
        }
        GuardianAssessmentAction::ApplyPatch { files, cwd: _ } => Some(if files.len() == 1 {
            format!("apply_patch touching {}", files[0])
        } else {
            format!("apply_patch touching {} files", files.len())
        }),
        GuardianAssessmentAction::NetworkAccess { target, host, .. } => {
            let label = if target.is_empty() { host } else { target };
            Some(format!("network access to {label}"))
        }
        GuardianAssessmentAction::McpToolCall {
            server,
            tool_name,
            connector_name,
            ..
        } => {
            let label = connector_name.as_deref().unwrap_or(server.as_str());
            Some(format!("MCP {tool_name} on {label}"))
        }
        GuardianAssessmentAction::RequestPermissions { reason, .. } => Some(
            reason
                .clone()
                .unwrap_or_else(|| "request additional permissions".to_string()),
        ),
        _ => None,
    }
}

pub(super) fn guardian_command_source_label(source: &GuardianCommandSource) -> &'static str {
    match source {
        GuardianCommandSource::Shell => "shell",
        GuardianCommandSource::UnifiedExec => "exec",
    }
}
