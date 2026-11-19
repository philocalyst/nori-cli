//! Translation of `antigravity-sdk-rust` `Step` events into ACP `SessionUpdate`s.

use std::collections::HashMap;

use antigravity_sdk_rust::types::Step;
use antigravity_sdk_rust::types::StepSource;
use antigravity_sdk_rust::types::StepStatus;
use antigravity_sdk_rust::types::StepTarget;
use antigravity_sdk_rust::types::StepType;
use antigravity_sdk_rust::types::ToolCall as SdkToolCall;
use nori_acp_kit::HostBridge;
use nori_acp_kit::Outcome;
use nori_acp_kit::ToolLedger;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::ContentChunk;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolKind;
use nori_acp_kit::value_content;
use serde_json::Value;
use serde_json::json;

/// Map an Antigravity built-in or host tool name onto an ACP [`ToolKind`].
pub(crate) fn tool_kind(name: &str) -> ToolKind {
    match name {
        "RUN_COMMAND" => ToolKind::Execute,
        "VIEW_FILE" => ToolKind::Read,
        "CREATE_FILE" | "EDIT_FILE" => ToolKind::Edit,
        "FIND_FILE" | "SEARCH_DIR" | "LIST_DIR" => ToolKind::Search,
        "SEARCH_WEB" | "READ_URL_CONTENT" => ToolKind::Fetch,
        "START_SUBAGENT" => ToolKind::Think,
        _ => ToolKind::Other,
    }
}

/// Stateful per-turn translator converting SDK [`Step`]s into ordered ACP [`SessionUpdate`]s.
#[derive(Default)]
pub(crate) struct StepTranslator {
    sent_thinking: HashMap<u32, usize>,
    sent_text: HashMap<u32, usize>,
    ledger: ToolLedger,
}

impl StepTranslator {
    /// Translate one step update into zero or more ACP session updates.
    pub(crate) fn translate_step(
        &mut self,
        step: &Step,
        host: Option<&HostBridge>,
    ) -> Vec<SessionUpdate> {
        let mut updates = Vec::new();

        let thought_delta = extract_delta(
            &step.thinking_delta,
            &step.thinking,
            &mut self.sent_thinking,
            step.step_index,
        );
        if !thought_delta.is_empty() {
            updates.push(SessionUpdate::AgentThoughtChunk(ContentChunk::new(
                ContentBlock::Text(TextContent::new(thought_delta)),
            )));
        }

        let is_model_text = step.source == StepSource::Model
            && step.target != StepTarget::Environment
            && step.r#type != StepType::ToolCall;
        if is_model_text {
            let text_delta = extract_delta(
                &step.content_delta,
                &step.content,
                &mut self.sent_text,
                step.step_index,
            );
            if !text_delta.is_empty() {
                updates.push(SessionUpdate::AgentMessageChunk(ContentChunk::new(
                    ContentBlock::Text(TextContent::new(text_delta)),
                )));
            }
        }

        for call in &step.tool_calls {
            let id = tool_call_id(step, call);
            let opening = ToolCall::new(id, call.name.clone())
                .kind(tool_kind(&call.name))
                .meta(host.and_then(|host| host.call_meta(&call.name)));
            match step.status {
                StepStatus::Active | StepStatus::WaitingForUser | StepStatus::Unknown => {
                    let status = if step.status == StepStatus::WaitingForUser {
                        ToolCallStatus::Pending
                    } else {
                        ToolCallStatus::InProgress
                    };
                    updates.extend(
                        self.ledger
                            .open(opening.status(status).raw_input(call.args.clone())),
                    );
                }
                StepStatus::Done => {
                    // A finished call carries its result in `args`.
                    let content = value_content(&call.args, &step.content);
                    let raw_output = if call.args.is_null() && !step.content.is_empty() {
                        Value::String(step.content.clone())
                    } else {
                        call.args.clone()
                    };
                    let meta = opening.meta.clone();
                    updates.extend(self.ledger.close(
                        opening,
                        Outcome::Completed,
                        content,
                        Some(raw_output),
                        meta,
                    ));
                }
                StepStatus::Error | StepStatus::Canceled | StepStatus::TerminalError => {
                    let message = if step.error.is_empty() {
                        "Tool execution failed".to_owned()
                    } else {
                        step.error.clone()
                    };
                    let meta = opening.meta.clone();
                    updates.extend(self.ledger.close(
                        opening.raw_input(call.args.clone()),
                        Outcome::Failed,
                        vec![ToolCallContent::from(message.clone())],
                        Some(json!({"error": message})),
                        meta,
                    ));
                }
            }
        }

        updates
    }
}

/// Build ACP replay updates for a persisted historical step during `session/load`.
pub(crate) fn replay_step_updates(step: &Step, host: Option<&HostBridge>) -> Vec<SessionUpdate> {
    if step.source == StepSource::User {
        if step.content.is_empty() {
            return Vec::new();
        }
        return vec![SessionUpdate::UserMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new(step.content.clone())),
        ))];
    }
    let mut translator = StepTranslator::default();
    translator.translate_step(step, host)
}

pub(crate) fn tool_call_id(step: &Step, tc: &SdkToolCall) -> String {
    if tc.id.is_empty() {
        format!("{}_{}", step.trajectory_id, step.step_index)
    } else {
        tc.id.clone()
    }
}

fn extract_delta(
    explicit_delta: &str,
    cumulative: &str,
    sent_offsets: &mut HashMap<u32, usize>,
    step_index: u32,
) -> String {
    if !explicit_delta.is_empty() {
        let prev = sent_offsets.get(&step_index).copied().unwrap_or(0);
        let next = cumulative.len().max(prev + explicit_delta.len());
        sent_offsets.insert(step_index, next);
        return explicit_delta.to_owned();
    }
    let prev = sent_offsets.get(&step_index).copied().unwrap_or(0);
    if cumulative.len() > prev && cumulative.is_char_boundary(prev) {
        sent_offsets.insert(step_index, cumulative.len());
        return cumulative[prev..].to_owned();
    }
    String::new()
}
