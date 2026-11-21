//! Each wire against a fake provider: what is sent, what streams back, what is assembled.

use nori_llm::ApiError;
use nori_llm::Auth;
use nori_llm::Client;
use nori_llm::Delta;
use nori_llm::Endpoint;
use nori_llm::ErrorKind;
use nori_llm::Message;
use nori_llm::Part;
use nori_llm::Reasoning;
use nori_llm::ReasoningPart;
use nori_llm::ReasoningProof;
use nori_llm::Request;
use nori_llm::Response;
use nori_llm::Secret;
use nori_llm::Stop;
use nori_llm::ToolCall;
use nori_llm::ToolResult;
use nori_llm::ToolSpec;
use nori_llm::Usage;
use nori_llm::Wire;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn sse(events: &[(&str, Value)]) -> String {
    events
        .iter()
        .map(|(name, data)| {
            if name.is_empty() {
                format!("data: {data}\n\n")
            } else {
                format!("event: {name}\ndata: {data}\n\n")
            }
        })
        .collect()
}

fn streamed(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn read_tool() -> ToolSpec {
    ToolSpec {
        name: "read".to_owned(),
        description: "Read a file".to_owned(),
        input_schema: json!({"type": "object", "properties": {"filePath": {"type": "string"}}}),
    }
}

/// A conversation with one finished tool round, so every part kind is encoded.
fn request(origin: &str, proof: ReasoningProof) -> Request {
    Request {
        model: "m".to_owned(),
        origin: origin.to_owned(),
        system: vec!["You are a coding agent.".to_owned()],
        messages: vec![
            Message::user(vec![Part::text("read a.txt")]),
            Message::assistant(vec![
                Part::Reasoning(ReasoningPart {
                    text: "I should read it".to_owned(),
                    proof,
                    origin: origin.to_owned(),
                }),
                Part::ToolCall(ToolCall {
                    id: "call_1".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"filePath": "a.txt"}),
                }),
            ]),
            Message::user(vec![Part::ToolResult(ToolResult::text(
                "call_1", "hello", false,
            ))]),
        ],
        tools: vec![read_tool()],
        max_output_tokens: Some(4096),
        ..Request::default()
    }
}

/// What a response says, without its headers.
#[derive(Debug, PartialEq)]
struct Answer {
    message: Message,
    stop: Stop,
    usage: Usage,
}

async fn run(endpoint: &Endpoint, request: &Request) -> (Result<Answer, ApiError>, Vec<Delta>) {
    let mut deltas = Vec::new();
    let result = Client::new()
        .stream(endpoint, request, &mut |delta| deltas.push(delta))
        .await
        .map(|response: Response| Answer {
            message: response.message,
            stop: response.stop,
            usage: response.usage,
        });
    (result, deltas)
}

async fn sent_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap_or_default();
    requests
        .first()
        .and_then(|request| serde_json::from_slice(&request.body).ok())
        .unwrap_or(Value::Null)
}

#[tokio::test]
async fn anthropic_streams_thinking_text_and_a_tool_call() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 12, "cache_read_input_tokens": 100, "cache_creation_input_tokens": 5, "output_tokens": 1}}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "Let me "}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "look."}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-1"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text", "text": ""}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "Reading."}}),
        ),
        (
            "content_block_start",
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "tool_use", "id": "toolu_2", "name": "read", "input": {}}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "{\"filePath\":"}}),
        ),
        (
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "input_json_delta", "partial_json": "\"b.txt\"}"}}),
        ),
        (
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 2}),
        ),
        (
            "message_delta",
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 40}}),
        ),
        ("message_stop", json!({"type": "message_stop"})),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(streamed(body))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(
        Wire::AnthropicMessages,
        format!("{}/v1", server.uri()),
        Auth::Header {
            name: "x-api-key".to_owned(),
            value: Secret::new("test-key"),
        },
    );
    let mut request = request(
        "anthropic/m",
        ReasoningProof::AnthropicSignature {
            signature: "sig-0".to_owned(),
        },
    );
    request.reasoning = Some(Reasoning::Budget(8000));
    let (result, deltas) = run(&endpoint, &request).await;
    assert_eq!(
        result,
        Ok(Answer {
            message: Message::assistant(vec![
                Part::Reasoning(ReasoningPart {
                    text: "Let me look.".to_owned(),
                    proof: ReasoningProof::AnthropicSignature {
                        signature: "sig-1".to_owned()
                    },
                    origin: "anthropic/m".to_owned(),
                }),
                Part::text("Reading."),
                Part::ToolCall(ToolCall {
                    id: "toolu_2".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"filePath": "b.txt"}),
                }),
            ]),
            stop: Stop::ToolUse,
            usage: Usage {
                input: 12,
                output: 40,
                cache_read: 100,
                cache_write: 5,
                reasoning: 0,
            },
        })
    );
    assert_eq!(
        deltas,
        vec![
            Delta::Reasoning("Let me ".to_owned()),
            Delta::Reasoning("look.".to_owned()),
            Delta::Text("Reading.".to_owned()),
            Delta::ToolCallStarted {
                id: "toolu_2".to_owned(),
                name: "read".to_owned()
            },
            Delta::ToolCallInput {
                id: "toolu_2".to_owned(),
                delta: "{\"filePath\":".to_owned()
            },
            Delta::ToolCallInput {
                id: "toolu_2".to_owned(),
                delta: "\"b.txt\"}".to_owned()
            },
        ]
    );
    let sent = sent_body(&server).await;
    assert_eq!(
        sent["thinking"],
        json!({"type": "enabled", "budget_tokens": 4095})
    );
    assert_eq!(
        sent["messages"][1]["content"],
        json!([
            {"type": "thinking", "thinking": "I should read it", "signature": "sig-0"},
            {"type": "tool_use", "id": "call_1", "name": "read", "input": {"filePath": "a.txt"}, "cache_control": {"type": "ephemeral"}},
        ])
    );
    assert_eq!(
        sent["messages"][2]["content"][0]["type"],
        json!("tool_result")
    );
    assert_eq!(sent["tools"][0]["name"], json!("read"));
}

#[tokio::test]
async fn anthropic_overload_mid_stream_is_a_retryable_error_in_prose() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "message_start",
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
        ),
        (
            "error",
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}),
        ),
    ]);
    Mock::given(method("POST"))
        .respond_with(streamed(body))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(Wire::AnthropicMessages, server.uri(), Auth::None);
    let (result, _) = run(&endpoint, &request("anthropic/m", ReasoningProof::None)).await;
    let error = result.err().unwrap_or_else(|| ApiError {
        kind: ErrorKind::Decode,
        status: None,
        message: "no error".to_owned(),
        retry_after: None,
        code: None,
    });
    assert_eq!(
        (error.kind, error.message.as_str(), error.is_retryable()),
        (ErrorKind::Overloaded, "Overloaded", true)
    );
}

#[tokio::test]
async fn http_errors_carry_the_providers_sentence_and_retry_after() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "7")
                .set_body_string(r#"{"error":{"message":"Rate limit reached for requests","type":"rate_limit_error"}}"#),
        )
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(Wire::OpenAiChat, server.uri(), Auth::None);
    let (result, _) = run(&endpoint, &request("openrouter/m", ReasoningProof::None)).await;
    assert_eq!(
        result.err(),
        Some(ApiError {
            kind: ErrorKind::RateLimited,
            status: Some(429),
            message: "Rate limit reached for requests".to_owned(),
            retry_after: Some(7),
            code: Some("rate_limit_error".to_owned()),
        })
    );
}

#[tokio::test]
async fn responses_completed_output_replaces_partial_stream_items() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "response.output_item.done",
            json!({"type":"response.output_item.done", "output_index":0, "item":{"type":"message", "content":[{"type":"output_text", "text":"partial"}]}}),
        ),
        (
            "response.completed",
            json!({"type":"response.completed", "response":{"output":[{"type":"message", "content":[{"type":"output_text", "text":"complete"}]}, {"type":"function_call", "call_id":"call_1", "name":"read", "arguments":"{}"}]}}),
        ),
    ]);
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(streamed(body))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(Wire::OpenAiResponses, server.uri(), Auth::None);
    let (result, _) = run(&endpoint, &Request::default()).await;
    let answer = result.unwrap();
    assert_eq!(answer.message.text(), "complete");
    assert_eq!(answer.message.tool_calls().count(), 1);
    assert_eq!(answer.stop, Stop::ToolUse);
}

#[tokio::test]
async fn responses_streams_reasoning_text_and_a_function_call() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "response.created",
            json!({"type": "response.created", "response": {}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1"}}),
        ),
        (
            "response.reasoning_summary_part.added",
            json!({"type": "response.reasoning_summary_part.added", "item_id": "rs_1"}),
        ),
        (
            "response.reasoning_summary_text.delta",
            json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "Thinking"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 0, "item": {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "Thinking"}], "encrypted_content": "enc-1"}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "message", "id": "msg_1"}}),
        ),
        (
            "response.output_text.delta",
            json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "Sure."}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 1, "item": {"type": "message", "id": "msg_1", "content": [{"type": "output_text", "text": "Sure."}]}}),
        ),
        (
            "response.output_item.added",
            json!({"type": "response.output_item.added", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "read"}}),
        ),
        (
            "response.function_call_arguments.delta",
            json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"filePath\":\"c\"}"}),
        ),
        (
            "response.output_item.done",
            json!({"type": "response.output_item.done", "output_index": 2, "item": {"type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "read", "arguments": "{\"filePath\":\"c\"}"}}),
        ),
        (
            "response.completed",
            json!({"type": "response.completed", "response": {"usage": {"input_tokens": 50, "input_tokens_details": {"cached_tokens": 20}, "output_tokens": 9, "output_tokens_details": {"reasoning_tokens": 4}}}}),
        ),
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .and(header("authorization", "Bearer sk-test"))
        .respond_with(streamed(body))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(
        Wire::OpenAiResponses,
        format!("{}/v1", server.uri()),
        Auth::Bearer(Secret::new("sk-test")),
    );
    let mut request = request(
        "openai/m",
        ReasoningProof::OpenAiEncrypted {
            id: "rs_0".to_owned(),
            content: "enc-0".to_owned(),
        },
    );
    request.reasoning = Some(Reasoning::Effort("high".to_owned()));
    let (result, deltas) = run(&endpoint, &request).await;
    assert_eq!(
        result,
        Ok(Answer {
            message: Message::assistant(vec![
                Part::Reasoning(ReasoningPart {
                    text: "Thinking".to_owned(),
                    proof: ReasoningProof::OpenAiEncrypted {
                        id: "rs_1".to_owned(),
                        content: "enc-1".to_owned()
                    },
                    origin: "openai/m".to_owned(),
                }),
                Part::text("Sure."),
                Part::ToolCall(ToolCall {
                    id: "call_9".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"filePath": "c"}),
                }),
            ]),
            stop: Stop::ToolUse,
            usage: Usage {
                input: 30,
                output: 9,
                cache_read: 20,
                cache_write: 0,
                reasoning: 4,
            },
        })
    );
    assert_eq!(deltas.len(), 4);
    let sent = sent_body(&server).await;
    assert_eq!(
        sent["input"],
        json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "read a.txt"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "I should read it"}], "encrypted_content": "enc-0"},
            {"type": "function_call", "call_id": "call_1", "name": "read", "arguments": "{\"filePath\":\"a.txt\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "hello"},
        ])
    );
    assert_eq!(
        (&sent["store"], &sent["reasoning"], &sent["include"]),
        (
            &json!(false),
            &json!({"effort": "high", "summary": "auto"}),
            &json!(["reasoning.encrypted_content"])
        )
    );
}

#[tokio::test]
async fn reasoning_from_another_model_is_not_sent_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(streamed(sse(&[(
            "",
            json!({"choices": [{"delta": {"content": "ok"}, "finish_reason": "stop"}]}),
        )])))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(Wire::OpenAiResponses, server.uri(), Auth::None);
    let mut request = request(
        "openai/m",
        ReasoningProof::OpenAiEncrypted {
            id: "rs_0".to_owned(),
            content: "enc-0".to_owned(),
        },
    );
    request.origin = "openai/other".to_owned();
    let _ = run(&endpoint, &request).await;
    let sent = sent_body(&server).await;
    let kinds: Vec<&str> = sent["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["type"].as_str())
        .collect();
    assert_eq!(
        kinds,
        vec!["message", "function_call", "function_call_output"]
    );
}

#[tokio::test]
async fn chat_assembles_tool_calls_split_across_chunks() {
    let server = MockServer::start().await;
    let body = sse(&[
        (
            "",
            json!({"choices": [{"delta": {"role": "assistant", "reasoning_content": "hmm"}}]}),
        ),
        ("", json!({"choices": [{"delta": {"content": "On it."}}]})),
        (
            "",
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_a", "type": "function", "function": {"name": "read", "arguments": ""}}]}}]}),
        ),
        (
            "",
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "{\"filePath\""}}]}}]}),
        ),
        (
            "",
            json!({"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": ":\"d\"}"}}]}}]}),
        ),
        (
            "",
            json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
        ),
        (
            "",
            json!({"choices": [], "usage": {"prompt_tokens": 70, "prompt_tokens_details": {"cached_tokens": 60}, "completion_tokens": 11}}),
        ),
    ]) + "data: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/api/v1/chat/completions"))
        .respond_with(streamed(body))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(
        Wire::OpenAiChat,
        format!("{}/api/v1", server.uri()),
        Auth::Bearer(Secret::new("or-key")),
    );
    let mut request = request("deepseek/m", ReasoningProof::None);
    request.reasoning_echo_field = Some("reasoning_content".to_owned());
    let (result, _) = run(&endpoint, &request).await;
    assert_eq!(
        result,
        Ok(Answer {
            message: Message::assistant(vec![
                Part::Reasoning(ReasoningPart {
                    text: "hmm".to_owned(),
                    proof: ReasoningProof::None,
                    origin: "deepseek/m".to_owned(),
                }),
                Part::text("On it."),
                Part::ToolCall(ToolCall {
                    id: "call_a".to_owned(),
                    name: "read".to_owned(),
                    input: json!({"filePath": "d"}),
                }),
            ]),
            stop: Stop::ToolUse,
            usage: Usage {
                input: 10,
                output: 11,
                cache_read: 60,
                cache_write: 0,
                reasoning: 0,
            },
        })
    );
    let sent = sent_body(&server).await;
    assert_eq!(
        sent["messages"],
        json!([
            {"role": "system", "content": "You are a coding agent."},
            {"role": "user", "content": "read a.txt"},
            {"role": "assistant", "content": null, "reasoning_content": "I should read it", "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "read", "arguments": "{\"filePath\":\"a.txt\"}"}}
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "hello"},
        ])
    );
}

#[tokio::test]
async fn a_stream_that_breaks_off_is_a_network_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(streamed(sse(&[(
            "",
            json!({"choices": [{"delta": {"content": "par"}}]}),
        )])))
        .mount(&server)
        .await;
    let endpoint = Endpoint::new(Wire::OpenAiChat, server.uri(), Auth::None);
    let (result, deltas) = run(&endpoint, &request("x/m", ReasoningProof::None)).await;
    assert_eq!(deltas, vec![Delta::Text("par".to_owned())]);
    assert_eq!(
        result.err().map(|error| error.kind),
        Some(ErrorKind::Network)
    );
}
