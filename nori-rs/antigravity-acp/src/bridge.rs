//! What the SDK asks of the client while a turn runs: tool permission, questions, MCP servers.

use std::sync::Arc;
use std::sync::RwLock;

use antigravity_sdk_rust::context::HookContext;
use antigravity_sdk_rust::hooks::Hook;
use antigravity_sdk_rust::types::AskQuestionEntry;
use antigravity_sdk_rust::types::HookResult;
use antigravity_sdk_rust::types::McpServerConfig;
use antigravity_sdk_rust::types::QuestionHookResult;
use antigravity_sdk_rust::types::QuestionResponse;
use antigravity_sdk_rust::types::StepSource;
use antigravity_sdk_rust::types::SystemInstructionSection;
use antigravity_sdk_rust::types::ToolCall as SdkToolCall;
use nori_acp_kit::HostBridge;
use nori_acp_kit::McpEndpoint;
use nori_acp_kit::McpTransport;
use nori_acp_kit::PermissionDecision;
use nori_acp_kit::Sink;
use nori_acp_kit::acp::UntypedMessage;
use nori_acp_kit::schema::PermissionOption;
use nori_acp_kit::schema::PermissionOptionKind;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use serde_json::Value;
use serde_json::json;

use crate::connection::ForkSeed;
use crate::translate::tool_kind;

/// The client a running turn talks to. Set at the start of each prompt, because the SDK's hooks
/// outlive any one request.
pub(crate) type SinkSlot = Arc<RwLock<Option<Sink>>>;

pub(crate) struct AcpBridgeHook {
    pub(crate) sink: SinkSlot,
    pub(crate) host: Option<HostBridge>,
}

impl AcpBridgeHook {
    fn current_sink(&self) -> Option<Sink> {
        self.sink.read().ok().and_then(|guard| guard.clone())
    }
}

impl Hook for AcpBridgeHook {
    async fn pre_tool_call(
        &self,
        tool_call: &SdkToolCall,
        _context: &HookContext,
    ) -> Result<HookResult, anyhow::Error> {
        // Nori's own tools carry Nori's own policy.
        if self
            .host
            .as_ref()
            .is_some_and(|host| host.is_host_tool(&tool_call.name))
        {
            return Ok(HookResult {
                allow: true,
                message: String::new(),
            });
        }
        let Some(sink) = self.current_sink() else {
            return Ok(HookResult {
                allow: false,
                message: "No ACP client attached for tool confirmation".to_owned(),
            });
        };
        let allow = request_permission(&sink, tool_call).await;
        Ok(HookResult {
            allow,
            message: if allow {
                String::new()
            } else {
                "Tool execution denied by user".to_owned()
            },
        })
    }

    async fn on_interaction(
        &self,
        questions: &[AskQuestionEntry],
        _context: &HookContext,
    ) -> Result<Option<QuestionHookResult>, anyhow::Error> {
        let Some(sink) = self.current_sink() else {
            return Ok(None);
        };
        Ok(Some(
            request_questions(&sink, "interaction", questions).await,
        ))
    }
}

/// Ask the client whether the tool call may run.
async fn request_permission(sink: &Sink, call: &SdkToolCall) -> bool {
    let call_id = if call.id.is_empty() {
        call.name.clone()
    } else {
        call.id.clone()
    };
    let tool = ToolCallUpdate::new(
        call_id,
        ToolCallUpdateFields::new()
            .title(call.name.clone())
            .kind(tool_kind(&call.name))
            .raw_input(call.args.clone()),
    );
    let options = vec![
        PermissionOption::new("once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new("always", "Always allow", PermissionOptionKind::AllowAlways),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ];
    matches!(
        sink.ask_permission(tool, options).await,
        Ok(PermissionDecision::Selected(id)) if matches!(id.0.as_ref(), "once" | "always")
    )
}

/// Forward interactive questions to the client via `_nori/request_user_input`.
pub(crate) async fn request_questions(
    sink: &Sink,
    call_id: &str,
    questions: &[AskQuestionEntry],
) -> QuestionHookResult {
    let encoded: Vec<Value> = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let options: Option<Vec<Value>> = (!question.options.is_empty()).then(|| {
                question
                    .options
                    .iter()
                    .map(|option| json!({"label": option.text, "description": ""}))
                    .collect()
            });
            json!({
                "id": format!("q{index}"),
                "header": "Question",
                "question": question.question,
                "isOther": true,
                "isSecret": false,
                "options": options,
                "multiple": question.is_multi_select,
            })
        })
        .collect();
    let params = json!({
        "sessionId": sink.session_id().to_string(),
        "callId": call_id,
        "turnId": call_id,
        "questions": encoded,
    });
    let Ok(message) = UntypedMessage::new("_nori/request_user_input", params) else {
        return cancelled_questions(questions.len());
    };
    match sink.cx().send_request(message).block_task().await {
        Ok(value) => parse_question_response(questions, &value),
        Err(_) => cancelled_questions(questions.len()),
    }
}

fn skipped() -> QuestionResponse {
    QuestionResponse {
        selected_option_ids: None,
        freeform_response: String::new(),
        skipped: true,
    }
}

fn cancelled_questions(len: usize) -> QuestionHookResult {
    QuestionHookResult {
        responses: vec![skipped(); len],
        cancelled: true,
    }
}

fn parse_question_response(questions: &[AskQuestionEntry], value: &Value) -> QuestionHookResult {
    if value["cancelled"].as_bool() == Some(true) {
        return cancelled_questions(questions.len());
    }
    let responses = questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            let Some(answers) = value["answers"][format!("q{index}")]["answers"].as_array() else {
                return skipped();
            };
            let mut selected = Vec::new();
            let mut freeform = Vec::new();
            for label in answers.iter().filter_map(Value::as_str) {
                match question.options.iter().find(|option| option.text == label) {
                    Some(option) => selected.push(option.id.clone()),
                    None => freeform.push(label.to_owned()),
                }
            }
            QuestionResponse {
                selected_option_ids: (!selected.is_empty()).then_some(selected),
                freeform_response: freeform.join(", "),
                skipped: answers.is_empty() && freeform.is_empty(),
            }
        })
        .collect();
    QuestionHookResult {
        responses,
        cancelled: false,
    }
}

/// The client's MCP servers as the SDK configures them.
pub(crate) fn mcp_server_configs(endpoints: &[McpEndpoint]) -> Vec<McpServerConfig> {
    endpoints
        .iter()
        .map(|endpoint| {
            let name = endpoint.name.clone();
            match &endpoint.transport {
                McpTransport::Stdio { command, args, env } => McpServerConfig::Stdio {
                    name,
                    command: command.clone(),
                    args: args.clone(),
                    env: env.iter().cloned().collect(),
                    timeout_seconds: None,
                    enabled_tools: None,
                    disabled_tools: None,
                },
                McpTransport::Http { url, headers } => McpServerConfig::Http {
                    name,
                    url: url.clone(),
                    headers: (!headers.is_empty()).then(|| headers.iter().cloned().collect()),
                    timeout: 30.0,
                    sse_read_timeout: 300.0,
                    terminate_on_close: true,
                    enabled_tools: None,
                    disabled_tools: None,
                },
                McpTransport::Sse { url, headers } => McpServerConfig::Sse {
                    name,
                    url: url.clone(),
                    timeout_seconds: None,
                    headers: (!headers.is_empty()).then(|| headers.iter().cloned().collect()),
                    enabled_tools: None,
                    disabled_tools: None,
                },
            }
        })
        .collect()
}

/// A forked session's prior conversation, as a system-instruction section.
pub(crate) fn fork_context(seed: Option<&ForkSeed>) -> Option<SystemInstructionSection> {
    let seed = seed.filter(|seed| !seed.history.is_empty())?;
    let transcript = seed
        .history
        .iter()
        .filter(|step| !step.content.trim().is_empty())
        .map(|step| {
            let role = match step.source {
                StepSource::User => "User",
                StepSource::Model => "Assistant",
                StepSource::System | StepSource::Unknown => "System",
            };
            format!("{role}: {}", step.content.trim())
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!transcript.is_empty()).then(|| SystemInstructionSection {
        title: "Forked Session Context".to_owned(),
        content: format!(
            "This session was forked from `{}`. Prior conversation:\n{transcript}",
            seed.source_session_id
        ),
    })
}
