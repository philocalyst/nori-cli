//! OpenAI's Chat Completions API, streamed: the protocol most other providers speak.

use std::collections::BTreeMap;

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

/// A tool call as its chunks arrive.
#[derive(Default)]
struct PendingCall {
    id: String,
    name: String,
    arguments: String,
    announced: bool,
}

pub(crate) async fn stream(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    request: &Request,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response, ApiError> {
    let body = body(request);
    let response = crate::post(http, endpoint, "chat/completions", &[], &body).await?;
    let headers = crate::headers_of(&response);
    let mut events = Events::new(response, endpoint.timeouts.chunk);
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut calls: BTreeMap<i64, PendingCall> = BTreeMap::new();
    let mut usage = Usage::default();
    let mut finish: Option<String> = None;
    let mut done = false;
    while let Some(event) = events.next().await? {
        if event.raw.trim() == "[DONE]" {
            done = true;
            break;
        }
        let data = &event.data;
        if data.get("error").is_some_and(|error| !error.is_null()) {
            return Err(ApiError::from_body(data, None));
        }
        if let Some(reported) = data.get("usage").filter(|usage| usage.is_object()) {
            read_usage(reported, &mut usage);
        }
        let Some(choice) = data["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            continue;
        };
        let delta = &choice["delta"];
        if let Some(piece) = delta["content"].as_str().filter(|piece| !piece.is_empty()) {
            text.push_str(piece);
            on_delta(Delta::Text(piece.to_owned()));
        }
        if let Some(piece) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
            .filter(|piece| !piece.is_empty())
        {
            reasoning.push_str(piece);
            on_delta(Delta::Reasoning(piece.to_owned()));
        }
        for call in delta["tool_calls"].as_array().into_iter().flatten() {
            let index = call["index"]
                .as_i64()
                .unwrap_or_else(|| i64::try_from(calls.len()).unwrap_or(i64::MAX));
            let pending = calls.entry(index).or_default();
            if let Some(id) = call["id"].as_str().filter(|id| !id.is_empty()) {
                id.clone_into(&mut pending.id);
            }
            if let Some(name) = call["function"]["name"]
                .as_str()
                .filter(|name| !name.is_empty())
            {
                pending.name.push_str(name);
            }
            if !pending.announced && !pending.name.is_empty() {
                if pending.id.is_empty() {
                    pending.id = format!("call_{index}");
                }
                pending.announced = true;
                on_delta(Delta::ToolCallStarted {
                    id: pending.id.clone(),
                    name: pending.name.clone(),
                });
            }
            if let Some(piece) = call["function"]["arguments"].as_str() {
                pending.arguments.push_str(piece);
                if pending.announced && !piece.is_empty() {
                    on_delta(Delta::ToolCallInput {
                        id: pending.id.clone(),
                        delta: piece.to_owned(),
                    });
                }
            }
        }
        if let Some(reason) = choice["finish_reason"].as_str() {
            finish = Some(reason.to_owned());
        }
    }
    if !done && finish.is_none() {
        return Err(ApiError::disconnected("no finish reason and no [DONE]"));
    }
    let mut parts = Vec::new();
    if !reasoning.is_empty() {
        parts.push(Part::Reasoning(ReasoningPart {
            text: reasoning,
            proof: ReasoningProof::None,
            origin: request.origin.clone(),
        }));
    }
    if !text.is_empty() {
        parts.push(Part::Text { text });
    }
    for (index, call) in calls {
        if call.name.is_empty() {
            continue;
        }
        parts.push(Part::ToolCall(ToolCall {
            id: if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id
            },
            name: call.name,
            input: crate::parse_arguments(&call.arguments),
        }));
    }
    let has_calls = parts.iter().any(|part| matches!(part, Part::ToolCall(_)));
    let stop = match finish.as_deref() {
        Some("length") => Stop::MaxTokens,
        Some("content_filter") => Stop::Refusal,
        _ if has_calls => Stop::ToolUse,
        Some("stop" | "end_turn" | "eos") | None => Stop::EndTurn,
        Some("tool_calls" | "function_call") => Stop::EndTurn,
        Some(other) => Stop::Other(other.to_owned()),
    };
    Ok(Response {
        message: Message::assistant(parts),
        stop,
        usage,
        headers,
    })
}

fn read_usage(value: &Value, usage: &mut Usage) {
    let Some(prompt) = value["prompt_tokens"].as_i64() else {
        return;
    };
    let cached = value["prompt_tokens_details"]["cached_tokens"]
        .as_i64()
        .or_else(|| value["prompt_cache_hit_tokens"].as_i64())
        .unwrap_or(0);
    usage.input = (prompt - cached).max(0);
    usage.cache_read = cached;
    usage.cache_write = value["prompt_tokens_details"]["cache_write_tokens"]
        .as_i64()
        .unwrap_or(0);
    usage.output = value["completion_tokens"].as_i64().unwrap_or(0);
    usage.reasoning = value["completion_tokens_details"]["reasoning_tokens"]
        .as_i64()
        .unwrap_or(0);
}

/// The request body.
pub(crate) fn body(request: &Request) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.model));
    body.insert("stream".to_owned(), json!(true));
    body.insert("stream_options".to_owned(), json!({"include_usage": true}));
    body.insert("messages".to_owned(), Value::Array(messages(request)));
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_tokens".to_owned(), json!(max));
    }
    match &request.reasoning {
        Some(Reasoning::Effort(effort)) => {
            body.insert("reasoning_effort".to_owned(), json!(effort));
        }
        Some(Reasoning::Budget(_)) | None => {
            if let Some(temperature) = request.temperature {
                body.insert("temperature".to_owned(), json!(temperature));
            }
        }
    }
    let mut body = Value::Object(body);
    crate::merge_body(&mut body, &request.extra_body);
    body
}

fn messages(request: &Request) -> Vec<Value> {
    let mut encoded = Vec::new();
    let system = request
        .system
        .iter()
        .filter(|text| !text.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if !system.is_empty() {
        encoded.push(json!({"role": "system", "content": system}));
    }
    for message in &request.messages {
        match message.role {
            Role::User => user(message, &mut encoded),
            Role::Assistant => {
                if let Some(assistant) = assistant(message, request) {
                    encoded.push(assistant);
                }
            }
        }
    }
    encoded
}

fn image_url(media_type: &str, data: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{data}")}})
}

fn user(message: &Message, encoded: &mut Vec<Value>) {
    let mut images_from_tools = Vec::new();
    // Tool results answer the assistant message right before them, so they come first.
    for part in &message.parts {
        if let Part::ToolResult(result) = part {
            encoded.push(json!({
                "role": "tool",
                "tool_call_id": result.id,
                "content": result.text_content(),
            }));
            for part in &result.content {
                if let ToolResultPart::Image { media_type, data } = part {
                    images_from_tools.push(image_url(media_type, data));
                }
            }
        }
    }
    let mut content: Vec<Value> = Vec::new();
    let mut has_image = false;
    for part in &message.parts {
        match part {
            Part::Text { text } if !text.is_empty() => {
                content.push(json!({"type": "text", "text": text}));
            }
            Part::Image { media_type, data } => {
                has_image = true;
                content.push(image_url(media_type, data));
            }
            Part::Text { .. } | Part::Reasoning(_) | Part::ToolCall(_) | Part::ToolResult(_) => {}
        }
    }
    if !images_from_tools.is_empty() {
        has_image = true;
        content.insert(
            0,
            json!({"type": "text", "text": "Images returned by the tool calls above:"}),
        );
        for (offset, image) in images_from_tools.into_iter().enumerate() {
            content.insert(offset + 1, image);
        }
    }
    if content.is_empty() {
        return;
    }
    if has_image {
        encoded.push(json!({"role": "user", "content": content}));
    } else {
        let text = content
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        encoded.push(json!({"role": "user", "content": text}));
    }
}

fn assistant(message: &Message, request: &Request) -> Option<Value> {
    let text = message.text();
    let calls: Vec<Value> = message
        .tool_calls()
        .map(|call| {
            json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": crate::arguments_text(&call.input)},
            })
        })
        .collect();
    if text.is_empty() && calls.is_empty() {
        return None;
    }
    let mut encoded = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !calls.is_empty() {
        encoded["tool_calls"] = Value::Array(calls);
    }
    if let Some(field) = &request.reasoning_echo_field {
        let reasoning = message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Reasoning(reasoning) if reasoning.origin == request.origin => {
                    Some(reasoning.text.as_str())
                }
                Part::Reasoning(_)
                | Part::Text { .. }
                | Part::Image { .. }
                | Part::ToolCall(_)
                | Part::ToolResult(_) => None,
            })
            .collect::<Vec<_>>()
            .join("");
        if !reasoning.is_empty() {
            encoded[field.as_str()] = json!(reasoning);
        }
    }
    Some(encoded)
}
