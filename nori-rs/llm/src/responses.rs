//! OpenAI's Responses API, streamed, stateless (`store: false`): reasoning comes back encrypted
//! and is sent again with the conversation.

use std::collections::BTreeMap;
use std::collections::HashMap;

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

pub(crate) async fn stream(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    request: &Request,
    on_delta: &mut (dyn FnMut(Delta) + Send),
) -> Result<Response, ApiError> {
    let body = body(request);
    let response = crate::post(http, endpoint, "responses", &[], &body).await?;
    let headers = crate::headers_of(&response);
    let mut events = Events::new(response, endpoint.timeouts.chunk);
    // Finished output items by their position.
    let mut done: BTreeMap<i64, Value> = BTreeMap::new();
    // Function call item ids to their call ids, for argument deltas.
    let mut calls: HashMap<String, String> = HashMap::new();
    let mut summary_parts: HashMap<String, i64> = HashMap::new();
    let mut usage = Usage::default();
    let mut incomplete: Option<String> = None;
    let mut finished = false;
    while let Some(event) = events.next().await? {
        let data = &event.data;
        let kind = data["type"].as_str().unwrap_or(event.name.as_str());
        match kind {
            "response.output_item.added" => {
                let item = &data["item"];
                if item["type"] == "function_call" {
                    let call_id = item["call_id"].as_str().unwrap_or("").to_owned();
                    let item_id = item["id"].as_str().unwrap_or("").to_owned();
                    on_delta(Delta::ToolCallStarted {
                        id: call_id.clone(),
                        name: item["name"].as_str().unwrap_or("").to_owned(),
                    });
                    calls.insert(item_id, call_id);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                on_delta(Delta::Text(data["delta"].as_str().unwrap_or("").to_owned()));
            }
            "response.reasoning_summary_part.added" => {
                let item = data["item_id"].as_str().unwrap_or("").to_owned();
                let count = summary_parts.entry(item).or_default();
                if *count > 0 {
                    on_delta(Delta::Reasoning("\n\n".to_owned()));
                }
                *count += 1;
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                on_delta(Delta::Reasoning(
                    data["delta"].as_str().unwrap_or("").to_owned(),
                ));
            }
            "response.function_call_arguments.delta" => {
                let item = data["item_id"].as_str().unwrap_or("");
                if let Some(call_id) = calls.get(item) {
                    on_delta(Delta::ToolCallInput {
                        id: call_id.clone(),
                        delta: data["delta"].as_str().unwrap_or("").to_owned(),
                    });
                }
            }
            "response.output_item.done" => {
                let index = data["output_index"]
                    .as_i64()
                    .unwrap_or_else(|| i64::try_from(done.len()).unwrap_or(i64::MAX));
                done.insert(index, data["item"].clone());
            }
            "response.completed" | "response.incomplete" => {
                let response = &data["response"];
                read_usage(&response["usage"], &mut usage);
                if kind == "response.incomplete" {
                    incomplete = Some(
                        response["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("incomplete")
                            .to_owned(),
                    );
                }
                // A response that carries its output wins over what the stream assembled.
                if let Some(output) = response["output"]
                    .as_array()
                    .filter(|output| !output.is_empty())
                {
                    done.clear();
                    for (index, item) in output.iter().enumerate() {
                        done.insert(i64::try_from(index).unwrap_or(i64::MAX), item.clone());
                    }
                }
                finished = true;
                break;
            }
            "response.failed" => {
                let error = &data["response"]["error"];
                return Err(ApiError::from_body(&json!({"error": error}), None));
            }
            "error" => return Err(ApiError::from_body(data, None)),
            _ => {}
        }
    }
    if !finished {
        return Err(ApiError::disconnected("no response.completed event"));
    }
    let mut parts = Vec::new();
    let mut refused = false;
    for item in done.into_values() {
        match item["type"].as_str() {
            Some("message") => {
                for content in item["content"].as_array().into_iter().flatten() {
                    match content["type"].as_str() {
                        Some("output_text") => {
                            let text = content["text"].as_str().unwrap_or("");
                            if !text.is_empty() {
                                parts.push(Part::text(text));
                            }
                        }
                        Some("refusal") => {
                            refused = true;
                            parts.push(Part::text(content["refusal"].as_str().unwrap_or("")));
                        }
                        _ => {}
                    }
                }
            }
            Some("reasoning") => {
                let text = item["summary"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .chain(item["content"].as_array().into_iter().flatten())
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                let proof = match item["encrypted_content"].as_str() {
                    Some(content) => ReasoningProof::OpenAiEncrypted {
                        id: item["id"].as_str().unwrap_or("").to_owned(),
                        content: content.to_owned(),
                    },
                    None => ReasoningProof::None,
                };
                parts.push(Part::Reasoning(ReasoningPart {
                    text,
                    proof,
                    origin: request.origin.clone(),
                }));
            }
            Some("function_call") => parts.push(Part::ToolCall(ToolCall {
                id: item["call_id"].as_str().unwrap_or("").to_owned(),
                name: item["name"].as_str().unwrap_or("").to_owned(),
                input: crate::parse_arguments(item["arguments"].as_str().unwrap_or("")),
            })),
            _ => {}
        }
    }
    let stop = match incomplete.as_deref() {
        Some("max_output_tokens") => Stop::MaxTokens,
        Some("content_filter") => Stop::Refusal,
        Some(other) => Stop::Other(other.to_owned()),
        None if refused => Stop::Refusal,
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

fn read_usage(value: &Value, usage: &mut Usage) {
    let Some(input) = value["input_tokens"].as_i64() else {
        return;
    };
    let cached = value["input_tokens_details"]["cached_tokens"]
        .as_i64()
        .unwrap_or(0);
    usage.input = (input - cached).max(0);
    usage.cache_read = cached;
    usage.output = value["output_tokens"].as_i64().unwrap_or(0);
    usage.reasoning = value["output_tokens_details"]["reasoning_tokens"]
        .as_i64()
        .unwrap_or(0);
}

/// The request body.
pub(crate) fn body(request: &Request) -> Value {
    let mut body = Map::new();
    body.insert("model".to_owned(), json!(request.model));
    body.insert("stream".to_owned(), json!(true));
    body.insert("store".to_owned(), json!(false));
    let instructions = request
        .system
        .iter()
        .filter(|text| !text.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    if !instructions.is_empty() {
        body.insert("instructions".to_owned(), json!(instructions));
    }
    body.insert("input".to_owned(), Value::Array(input(request)));
    if !request.tools.is_empty() {
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.input_schema,
                    "strict": false,
                })
            })
            .collect();
        body.insert("tools".to_owned(), Value::Array(tools));
        body.insert("tool_choice".to_owned(), json!("auto"));
        body.insert("parallel_tool_calls".to_owned(), json!(true));
    }
    match &request.reasoning {
        Some(Reasoning::Effort(effort)) => {
            body.insert(
                "reasoning".to_owned(),
                json!({"effort": effort, "summary": "auto"}),
            );
            body.insert("include".to_owned(), json!(["reasoning.encrypted_content"]));
        }
        Some(Reasoning::Budget(_)) | None => {
            if let Some(temperature) = request.temperature {
                body.insert("temperature".to_owned(), json!(temperature));
            }
        }
    }
    if let Some(max) = request.max_output_tokens {
        body.insert("max_output_tokens".to_owned(), json!(max));
    }
    if let Some(key) = &request.cache_key {
        body.insert("prompt_cache_key".to_owned(), json!(key));
    }
    let mut body = Value::Object(body);
    crate::merge_body(&mut body, &request.extra_body);
    body
}

fn input(request: &Request) -> Vec<Value> {
    let mut items = Vec::new();
    for message in &request.messages {
        let mut content = Vec::new();
        let flush = |content: &mut Vec<Value>, items: &mut Vec<Value>| {
            if content.is_empty() {
                return;
            }
            let role = match message.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };
            items.push(json!({
                "type": "message",
                "role": role,
                "content": std::mem::take(content),
            }));
        };
        for part in &message.parts {
            match part {
                Part::Text { text } if text.is_empty() => {}
                Part::Text { text } => content.push(match message.role {
                    Role::User => json!({"type": "input_text", "text": text}),
                    Role::Assistant => json!({"type": "output_text", "text": text}),
                }),
                Part::Image { media_type, data } => content.push(json!({
                    "type": "input_image",
                    "image_url": format!("data:{media_type};base64,{data}"),
                })),
                Part::Reasoning(reasoning) => {
                    if let ReasoningProof::OpenAiEncrypted {
                        content: encrypted, ..
                    } = &reasoning.proof
                        && reasoning.origin == request.origin
                    {
                        flush(&mut content, &mut items);
                        let summary: Vec<Value> = if reasoning.text.is_empty() {
                            Vec::new()
                        } else {
                            vec![json!({"type": "summary_text", "text": reasoning.text})]
                        };
                        items.push(json!({
                            "type": "reasoning",
                            "summary": summary,
                            "encrypted_content": encrypted,
                        }));
                    }
                }
                Part::ToolCall(call) => {
                    flush(&mut content, &mut items);
                    items.push(json!({
                        "type": "function_call",
                        "call_id": call.id,
                        "name": call.name,
                        "arguments": crate::arguments_text(&call.input),
                    }));
                }
                Part::ToolResult(result) => {
                    flush(&mut content, &mut items);
                    let has_image = result
                        .content
                        .iter()
                        .any(|part| matches!(part, ToolResultPart::Image { .. }));
                    let output = if has_image {
                        Value::Array(
                            result
                                .content
                                .iter()
                                .map(|part| match part {
                                    ToolResultPart::Text { text } => {
                                        json!({"type": "input_text", "text": text})
                                    }
                                    ToolResultPart::Image { media_type, data } => json!({
                                        "type": "input_image",
                                        "image_url": format!("data:{media_type};base64,{data}"),
                                    }),
                                })
                                .collect(),
                        )
                    } else {
                        json!(result.text_content())
                    };
                    items.push(json!({
                        "type": "function_call_output",
                        "call_id": result.id,
                        "output": output,
                    }));
                }
            }
        }
        flush(&mut content, &mut items);
    }
    items
}
