//! Streaming clients for model providers' own APIs.
//!
//! One provider-neutral conversation ([`Message`]s of [`Part`]s), three wire protocols:
//!
//! - [`Wire::AnthropicMessages`]: Anthropic's Messages API, and every provider that copies it.
//! - [`Wire::OpenAiResponses`]: OpenAI's Responses API.
//! - [`Wire::OpenAiChat`]: OpenAI's Chat Completions API, which most other providers speak
//!   (OpenRouter, Groq, GitHub Copilot, DeepSeek, xAI, local servers, ...).
//!
//! [`Client::stream`] sends one request, reports what the model writes as it arrives
//! ([`Delta`]), and returns the assembled assistant [`Message`] with why it stopped and what it
//! cost ([`Response`]). Errors are [`ApiError`]s: a kind the caller's retry policy can act on and
//! a sentence a person can read, never the provider's raw body.
//!
//! The crate knows nothing about agents, tools or sessions; it is the wire and nothing else.

mod anthropic;
mod chat;
mod error;
mod responses;
mod sse;

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;

pub use error::ApiError;
pub use error::ErrorKind;

/// The wire protocol a provider speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    /// Anthropic's Messages API: `POST {base}/messages`.
    AnthropicMessages,
    /// OpenAI's Responses API: `POST {base}/responses`.
    OpenAiResponses,
    /// OpenAI's Chat Completions API: `POST {base}/chat/completions`.
    OpenAiChat,
}

/// A credential. Its value is never printed, logged or serialized.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value, for the one place that has to send it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// How a request proves who sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Auth {
    /// Nothing: a local server.
    None,
    /// `Authorization: Bearer <secret>`.
    Bearer(Secret),
    /// A provider's own header, such as Anthropic's `x-api-key` or Azure's `api-key`.
    Header { name: String, value: Secret },
}

/// How long to wait before a request counts as stalled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    /// From sending the request to the response's headers.
    pub headers: Duration,
    /// Between two chunks of a streamed response.
    pub chunk: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            headers: Duration::from_secs(300),
            chunk: Duration::from_secs(300),
        }
    }
}

/// Where a provider is and how to reach it.
#[derive(Clone)]
pub struct Endpoint {
    pub wire: Wire,
    /// The API root, such as `https://api.anthropic.com/v1`; the wire's path is appended.
    pub base_url: String,
    pub auth: Auth,
    /// Extra headers from provider or model settings. They may carry secrets.
    pub headers: Vec<(String, Secret)>,
    /// Query parameters, such as Azure's `api-version`.
    pub query: Vec<(String, String)>,
    pub timeouts: Timeouts,
}

impl Endpoint {
    pub fn new(wire: Wire, base_url: impl Into<String>, auth: Auth) -> Self {
        Self {
            wire,
            base_url: base_url.into(),
            auth,
            headers: Vec::new(),
            query: Vec::new(),
            timeouts: Timeouts::default(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url.trim_end_matches('/'))
    }
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("wire", &self.wire)
            .field("base_url", &self.base_url)
            .field(
                "headers",
                &self
                    .headers
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("query", &self.query)
            .finish_non_exhaustive()
    }
}

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

/// One message of the conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub parts: Vec<Part>,
}

impl Message {
    pub fn user(parts: Vec<Part>) -> Self {
        Self {
            role: Role::User,
            parts,
        }
    }

    pub fn assistant(parts: Vec<Part>) -> Self {
        Self {
            role: Role::Assistant,
            parts,
        }
    }

    /// The message's text, its text parts joined.
    pub fn text(&self) -> String {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::Text { text } => Some(text.as_str()),
                Part::Image { .. }
                | Part::Reasoning(_)
                | Part::ToolCall(_)
                | Part::ToolResult(_) => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    /// The tool calls the message makes.
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.parts.iter().filter_map(|part| match part {
            Part::ToolCall(call) => Some(call),
            Part::Text { .. } | Part::Image { .. } | Part::Reasoning(_) | Part::ToolResult(_) => {
                None
            }
        })
    }
}

/// A piece of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    /// An image, base64 encoded.
    Image {
        media_type: String,
        data: String,
    },
    Reasoning(ReasoningPart),
    ToolCall(ToolCall),
    ToolResult(ToolResult),
}

impl Part {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }
}

/// What a model thought before answering.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningPart {
    /// The readable reasoning (or its summary). May be empty when the provider keeps it private.
    pub text: String,
    /// What the provider needs back to continue from this reasoning. It is only valid for the
    /// model that produced it ([`ReasoningPart::origin`]).
    #[serde(default, skip_serializing_if = "ReasoningProof::is_none")]
    pub proof: ReasoningProof,
    /// The provider and model that produced it (`provider/model`), as the caller named them.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub origin: String,
}

/// The provider's own token for a piece of reasoning.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReasoningProof {
    #[default]
    None,
    /// Anthropic's signature over a thinking block.
    AnthropicSignature { signature: String },
    /// Anthropic's redacted thinking block.
    AnthropicRedacted { data: String },
    /// OpenAI's encrypted reasoning item.
    OpenAiEncrypted { id: String, content: String },
}

impl ReasoningProof {
    pub fn is_none(&self) -> bool {
        matches!(self, Self::None)
    }
}

/// A call the model made to a tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// The arguments: a JSON object when the model wrote valid JSON, else the raw text as a
    /// string.
    pub input: Value,
}

/// What a tool call returned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: Vec<ToolResultPart>,
    #[serde(default)]
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(id: impl Into<String>, text: impl Into<String>, is_error: bool) -> Self {
        Self {
            id: id.into(),
            content: vec![ToolResultPart::Text { text: text.into() }],
            is_error,
        }
    }

    /// The result's text parts, joined.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ToolResultPart::Text { text } => Some(text.as_str()),
                ToolResultPart::Image { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// A piece of a tool result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultPart {
    Text { text: String },
    Image { media_type: String, data: String },
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// The arguments' JSON Schema, an object schema.
    pub input_schema: Value,
}

/// How hard the model should think.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reasoning {
    /// An effort level the provider names: `minimal`, `low`, `medium`, `high`, ...
    Effort(String),
    /// A number of tokens to think with (Anthropic's extended thinking).
    Budget(i64),
}

/// One request to a model.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The model as the provider names it.
    pub model: String,
    /// `provider/model` as the caller names it: reasoning proofs from any other origin are not
    /// sent back, since a provider only accepts its own model's.
    pub origin: String,
    /// The system prompt, in parts (the first ones stable, so they can be cached).
    pub system: Vec<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_output_tokens: Option<i64>,
    pub reasoning: Option<Reasoning>,
    pub temperature: Option<f64>,
    /// Chat Completions only: the assistant message field that carries reasoning back to a model
    /// that wants it (`reasoning_content` for DeepSeek and Kimi). `None` drops reasoning.
    pub reasoning_echo_field: Option<String>,
    /// A key that groups requests sharing a prefix, so the provider can cache it.
    pub cache_key: Option<String>,
    /// Fields merged into the request body last: the provider's and model's own settings.
    pub extra_body: Map<String, Value>,
}

/// Something the model wrote, as it arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    /// The model began a tool call; its arguments follow.
    ToolCallStarted {
        id: String,
        name: String,
    },
    ToolCallInput {
        id: String,
        delta: String,
    },
}

/// Why the model stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It finished its answer.
    EndTurn,
    /// It wants the results of its tool calls.
    ToolUse,
    /// It ran out of output tokens.
    MaxTokens,
    /// It refused, or a content filter stopped it.
    Refusal,
    /// A reason this crate does not know, as the provider named it.
    Other(String),
}

/// Tokens one request used, as the provider reported them. `input` excludes cached input.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// Output tokens spent reasoning, when the provider separates them (included in `output`).
    pub reasoning: i64,
}

impl Usage {
    /// Everything the request put in the context window: what it read and what it wrote.
    pub fn context_tokens(&self) -> i64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }
}

/// What one request produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// The assistant message, assembled.
    pub message: Message,
    pub stop: Stop,
    pub usage: Usage,
    /// The response's headers (rate-limit windows and the like), cookies left out.
    pub headers: Vec<(String, String)>,
}

/// Sends requests. Cheap to clone; one per process is enough.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

impl Client {
    pub fn new() -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self { http }
    }

    /// Send `request` and stream the answer: every piece goes to `on_delta` as it arrives, and
    /// the assembled answer is returned. Dropping the future cancels the request.
    pub async fn stream(
        &self,
        endpoint: &Endpoint,
        request: &Request,
        on_delta: &mut (dyn FnMut(Delta) + Send),
    ) -> Result<Response, ApiError> {
        match endpoint.wire {
            Wire::AnthropicMessages => {
                anthropic::stream(&self.http, endpoint, request, on_delta).await
            }
            Wire::OpenAiResponses => {
                responses::stream(&self.http, endpoint, request, on_delta).await
            }
            Wire::OpenAiChat => chat::stream(&self.http, endpoint, request, on_delta).await,
        }
    }
}

/// Send `body` to `path` of `endpoint` and return the response once its headers arrive, or the
/// provider's error.
async fn post(
    http: &reqwest::Client,
    endpoint: &Endpoint,
    path: &str,
    extra_headers: &[(&str, &str)],
    body: &Value,
) -> Result<reqwest::Response, ApiError> {
    let mut builder = http
        .post(endpoint.url(path))
        .header("content-type", "application/json")
        .header("accept", "text/event-stream");
    builder = match &endpoint.auth {
        Auth::None => builder,
        Auth::Bearer(secret) => builder.bearer_auth(secret.expose()),
        Auth::Header { name, value } => builder.header(name.as_str(), value.expose()),
    };
    for (name, value) in extra_headers {
        builder = builder.header(*name, *value);
    }
    for (name, value) in &endpoint.headers {
        builder = builder.header(name.as_str(), value.expose());
    }
    if !endpoint.query.is_empty() {
        builder = builder.query(&endpoint.query);
    }
    let send = builder.json(body).send();
    let response = tokio::time::timeout(endpoint.timeouts.headers, send)
        .await
        .map_err(|_| ApiError::timeout(endpoint.timeouts.headers, "the response to begin"))?
        .map_err(ApiError::from_transport)?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let retry_after = error::retry_after(response.headers());
    let text = response.text().await.unwrap_or_default();
    Err(ApiError::from_http(i32::from(status), &text, retry_after))
}

/// A response's headers as text, cookies left out.
fn headers_of(response: &reqwest::Response) -> Vec<(String, String)> {
    response
        .headers()
        .iter()
        .filter(|(name, _)| !name.as_str().eq_ignore_ascii_case("set-cookie"))
        .filter_map(|(name, value)| {
            Some((name.as_str().to_owned(), value.to_str().ok()?.to_owned()))
        })
        .collect()
}

/// Merge `extra` into `body` (top-level keys replace, objects merge one level deep).
fn merge_body(body: &mut Value, extra: &Map<String, Value>) {
    let Some(object) = body.as_object_mut() else {
        return;
    };
    for (key, value) in extra {
        match (object.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(update)) => {
                for (inner_key, inner_value) in update {
                    existing.insert(inner_key.clone(), inner_value.clone());
                }
            }
            (Some(_) | None, _) => {
                object.insert(key.clone(), value.clone());
            }
        }
    }
}

/// Arguments the model streamed, as JSON when they parse and as the raw text otherwise.
fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return Value::Object(Map::new());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()))
}

/// The arguments as the JSON text a provider takes back.
fn arguments_text(input: &Value) -> String {
    match input {
        Value::String(raw) => raw.clone(),
        other => other.to_string(),
    }
}
