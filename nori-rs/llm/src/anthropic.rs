//! Anthropic's Messages API, streamed.

use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::ApiError;
use crate::Delta;
use crate::Endpoint;
use crate::Message;
use crate::Part;
use crate::Reasoning;
use crate::ReasoningPart;
use crate::ReasoningProof;
use crate::Request;
use crate::Response;
use crate::Role;
use crate::Stop;
use crate::ToolCall;
use crate::ToolResultPart;
use crate::Usage;
use crate::sse::Events;

const VERSION: &str = "2023-06-01";
/// Thinking between tool calls, for the models that do it.
const INTERLEAVED_THINKING: &str = "interleaved-thinking-2025-05-14";
/// What a request may produce when the caller does not say.
const DEFAULT_MAX_TOKENS: i64 = 8192;
/// The smallest thinking budget Anthropic accepts.
const MIN_BUDGET: i64 = 1024;

pub(crate) async fn stream(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    request: &Request,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response, ApiError> {
    let body = body(request);
    let thinking = body.get("thinking").is_some();
    let mut headers = vec![("anthropic-version", VERSION)];
    if thinking && !request.tools.is_empty() {
        headers.push(("anthropic-beta", INTERLEAVED_THINKING));
    }
    let response = crate::post(http, endpoint, "messages", &headers, &body).await?;
    let headers = crate::headers_of(&response);
    let mut events = Events::new(response, endpoint.timeouts.chunk);
    let mut blocks: Vec<Block> = Vec::new();
    let mut usage = Usage::default();
    let mut stop = None;
    let mut finished = false;
    while let Some(event) = events.next().await? {
        let data = &event.data;
        match data["type"].as_str().unwrap_or(event.name.as_str()) {
            "message_start" => read_usage(&data["message"]["usage"], &mut usage),
            "content_block_start" => {
                let index = index_of(data)?;
                let block = &data["content_block"];
                let started = match block["type"].as_str() {
                    Some("text") => Block::Text(block["text"].as_str().unwrap_or("").to_owned()),
                    Some("thinking") => Block::Thinking {
                        text: block["thinking"].as_str().unwrap_or("").to_owned(),
                        signature: String::new(),
                    },
                    Some("redacted_thinking") => {
                        Block::Redacted(block["data"].as_str().unwrap_or("").to_owned())
                    }
                    Some("tool_use") => {
                        let id = block["id"].as_str().unwrap_or("").to_owned();
                        let name = block["name"].as_str().unwrap_or("").to_owned();
                        on_delta(Delta::ToolCallStarted {
                            id: id.clone(),
                            name: name.clone(),
                        });
                        Block::ToolUse {
                            id,
                            name,
                            json: String::new(),
                        }
                    }
                    _ => Block::Ignored,
                };
                if blocks.len() <= index {
                    blocks.resize_with(index + 1, || Block::Ignored);
                }
                blocks[index] = started;
            }
            "content_block_delta" => {
                let index = index_of(data)?;
                let delta = &data["delta"];
                let Some(block) = blocks.get_mut(index) else {
                    continue;
                };
                match (block, delta["type"].as_str()) {
                    (Block::Text(text), Some("text_delta")) => {
                        let piece = delta["text"].as_str().unwrap_or("");
                        text.push_str(piece);
                        on_delta(Delta::Text(piece.to_owned()));
                    }
                    (Block::Thinking { text, .. }, Some("thinking_delta")) => {
                        let piece = delta["thinking"].as_str().unwrap_or("");
                        text.push_str(piece);
                        on_delta(Delta::Reasoning(piece.to_owned()));
                    }
                    (Block::Thinking { signature, .. }, Some("signature_delta")) => {
                        signature.push_str(delta["signature"].as_str().unwrap_or(""));
                    }
                    (Block::ToolUse { id, json, .. }, Some("input_json_delta")) => {
                        let piece = delta["partial_json"].as_str().unwrap_or("");
                        json.push_str(piece);
                        on_delta(Delta::ToolCallInput {
                            id: id.clone(),
                            delta: piece.to_owned(),
                        });
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = data["delta"]["stop_reason"].as_str() {
                    stop = Some(stop_reason(reason));
                }
                read_usage(&data["usage"], &mut usage);
            }
            "message_stop" => {
                finished = true;
                break;
            }
            "error" => return Err(ApiError::from_body(data, None)),
            _ => {}
        }
    }
    if !finished {
        return Err(ApiError::disconnected("no message_stop event"));
    }
    let origin = request.origin.clone();
    let parts = blocks
        .into_iter()
        .filter_map(|block| match block {
            Block::Text(text) => (!text.is_empty()).then_some(Part::Text { text }),
            Block::Thinking { text, signature } => Some(Part::Reasoning(ReasoningPart {
                text,
                proof: if signature.is_empty() {
                    ReasoningProof::None
                } else {
                    ReasoningProof::AnthropicSignature { signature }
                },
                origin: origin.clone(),
            })),
            Block::Redacted(data) => Some(Part::Reasoning(ReasoningPart {
                text: String::new(),
                proof: ReasoningProof::AnthropicRedacted { data },
                origin: origin.clone(),
            })),
            Block::ToolUse { id, name, json } => Some(Part::ToolCall(ToolCall {
                id,
                name,
                input: crate::parse_arguments(&json),
            })),
            Block::Ignored => None,
        })
        .collect::<Vec<_>>();
    let stop = match stop {
        Some(stop) => stop,
        None if parts.iter().any(|part| matches!(part, Part::ToolCall(_))) => Stop::ToolUse,
        None => Stop::EndTurn,
    };
    Ok(Response {
        message: Message::assistant(parts),
        stop,
        usage,
        headers,
    })
}

enum Block {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Redacted(String),
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Ignored,
}

fn index_of(data: &Value) -> Result<usize, ApiError> {
    data["index"]
        .as_u64()
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| ApiError::decode("Anthropic sent a content block without an index"))
}

fn read_usage(value: &Value, usage: &mut Usage) {
    let field = |name: &str| value[name].as_i64();
    if let Some(input) = field("input_tokens") {
        usage.input = input;
    }
    if let Some(output) = field("output_tokens") {
        usage.output = output;
    }
    if let Some(read) = field("cache_read_input_tokens") {
        usage.cache_read = read;
    }
    if let Some(write) = field("cache_creation_input_tokens") {
        usage.cache_write = write;
    }
}

fn stop_reason(reason: &str) -> Stop {
    match reason {
        "end_turn" | "stop_sequence" => Stop::EndTurn,
        "tool_use" => Stop::ToolUse,
        "max_tokens" | "model_context_window_exceeded" => Stop::MaxTokens,
        "refusal" => Stop::Refusal,
        other => Stop::Other(other.to_owned()),
    }
}

/// The request body.
pub(crate) fn body(request: &Request) -> Value {
    let mut max_tokens = request.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.model));
    body.insert("stream".to_owned(), json!(true));
    let mut system: Vec<Value> = request
        .system
        .iter()
        .filter(|text| !text.trim().is_empty())
        .map(|text| json!({"type": "text", "text": text}))
        .collect();
    if let Some(last) = system.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
    }
    if !system.is_empty() {
        body.insert("system".to_owned(), Value::Array(system));
    }
    let mut messages = messages(request);
    // Cache the conversation up to its last two messages, so the next request reads it back.
    for message in messages.iter_mut().rev().take(2) {
        if let Some(last) = message["content"]
            .as_array_mut()
            .and_then(|blocks| blocks.last_mut())
        {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    body.insert("messages".to_owned(), Value::Array(messages));
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "input_schema": tool.input_schema,
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    match &request.reasoning {
        Some(Reasoning::Budget(budget)) if *budget > 0 => {
            if max_tokens <= MIN_BUDGET {
                max_tokens = MIN_BUDGET * 2;
            }
            let budget = (*budget).clamp(MIN_BUDGET, max_tokens - 1);
            body.insert(
                "thinking".to_owned(),
                json!({"type": "enabled", "budget_tokens": budget}),
            );
        }
        Some(Reasoning::Effort(effort)) => {
            body.insert("output_config".to_owned(), json!({"effort": effort}));
        }
        Some(Reasoning::Budget(_)) | None => {
            if let Some(temperature) = request.temperature {
                body.insert("temperature".to_owned(), json!(temperature));
            }
        }
    }
    body.insert("max_tokens".to_owned(), json!(max_tokens));
    let mut body = Value::Object(body);
    crate::merge_body(&mut body, &request.extra_body);
    body
}

fn messages(request: &Request) -> Vec<Value> {
    let mut encoded: Vec<Value> = Vec::new();
    for message in &request.messages {
        let content = content(message, &request.origin);
        if content.is_empty() {
            continue;
        }
        let role = match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        // Consecutive messages of one role become one, as the API wants turns to alternate.
        if let Some(previous) = encoded.last_mut()
            && previous["role"] == role
            && let Some(blocks) = previous["content"].as_array_mut()
        {
            let results_first = role == "user";
            for block in content {
                if results_first && block["type"] == "tool_result" {
                    let at = blocks
                        .iter()
                        .take_while(|block| block["type"] == "tool_result")
                        .count();
                    blocks.insert(at, block);
                } else {
                    blocks.push(block);
                }
            }
            continue;
        }
        encoded.push(json!({"role": role, "content": content}));
    }
    encoded
}

fn content(message: &Message, origin: &str) -> Vec<Value> {
    let mut results = Vec::new();
    let mut blocks = Vec::new();
    for part in &message.parts {
        match part {
            Part::Text { text } if !text.is_empty() => {
                blocks.push(json!({"type": "text", "text": text}));
            }
            Part::Text { .. } => {}
            Part::Image { media_type, data } => blocks.push(image(media_type, data)),
            Part::Reasoning(reasoning) if reasoning.origin == origin => match &reasoning.proof {
                ReasoningProof::AnthropicSignature { signature } => blocks.push(json!({
                    "type": "thinking",
                    "thinking": reasoning.text,
                    "signature": signature,
                })),
                ReasoningProof::AnthropicRedacted { data } => {
                    blocks.push(json!({"type": "redacted_thinking", "data": data}));
                }
                ReasoningProof::None | ReasoningProof::OpenAiEncrypted { .. } => {}
            },
            Part::Reasoning(_) => {}
            Part::ToolCall(call) => {
                let input = match &call.input {
                    Value::Object(_) => call.input.clone(),
                    Value::String(raw) => serde_json::from_str::<Value>(raw)
                        .ok()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({})),
                    _ => json!({}),
                };
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": input,
                }));
            }
            Part::ToolResult(result) => {
                let content: Vec<Value> = result
                    .content
                    .iter()
                    .map(|part| match part {
                        ToolResultPart::Text { text } => json!({"type": "text", "text": text}),
                        ToolResultPart::Image { media_type, data } => image(media_type, data),
                    })
                    .collect();
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": result.id,
                    "content": content,
                });
                if result.is_error {
                    block["is_error"] = json!(true);
                }
                results.push(block);
            }
        }
    }
    // Tool results lead a user message.
    results.extend(blocks);
    results
}

fn image(media_type: &str, data: &str) -> Value {
    json!({
        "type": "image",
        "source": {"type": "base64", "media_type": media_type, "data": data},
    })
}
