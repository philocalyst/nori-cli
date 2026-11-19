//! The Gemini Core engine as a kit [`Provider`].

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nori_acp_kit::AgentError;
use nori_acp_kit::Capabilities;
use nori_acp_kit::Choice;
use nori_acp_kit::ContextUsage;
use nori_acp_kit::InProcessSpec;
use nori_acp_kit::McpTransport;
use nori_acp_kit::Opened;
use nori_acp_kit::Options;
use nori_acp_kit::Profile;
use nori_acp_kit::Provider;
use nori_acp_kit::ProviderAgent;
use nori_acp_kit::Registration;
use nori_acp_kit::Select;
use nori_acp_kit::Sink;
use nori_acp_kit::SpecOptions;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::ToolLedger;
use nori_acp_kit::TurnCtx;
use nori_acp_kit::UsageReport;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::EmbeddedResourceResource;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::PermissionOption;
use nori_acp_kit::schema::PermissionOptionKind;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::engine::Engine;

/// Offer the Gemini Core adapter to the host as the native agent `gemini`.
pub fn registration() -> Registration {
    Registration::new("gemini", |spec| async move {
        Gemini::from_spec(spec).map(ProviderAgent::new)
    })
}

/// Serve Gemini Core over one end of an in-memory pipe. `module` is the absolute path to the
/// installed official Core package entry point.
pub async fn serve_over_pipe(
    pipe: tokio::io::DuplexStream,
    node: PathBuf,
    module: PathBuf,
    environment: HashMap<OsString, OsString>,
) -> std::io::Result<()> {
    let agent = ProviderAgent::new(Gemini {
        node,
        module,
        environment,
    });
    nori_acp_kit::serve_over_pipe(pipe, agent).await
}

struct Gemini {
    node: PathBuf,
    module: PathBuf,
    environment: HashMap<OsString, OsString>,
}

impl Gemini {
    fn from_spec(spec: InProcessSpec) -> std::io::Result<Self> {
        let options =
            SpecOptions::parse("Gemini", &spec.config, &["node_path", "core_path", "auth"])?;
        let not_found =
            |message: String| std::io::Error::new(std::io::ErrorKind::NotFound, message);
        let mut environment = spec.environment_os();
        if let Some(home) = &spec.home {
            environment.insert("HOME".into(), home.into());
        }
        if let Some(auth) = options.get("auth") {
            if !matches!(auth, "gemini" | "vertex") {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "native Gemini supports auth=gemini or auth=vertex; use the official CLI for Google account login",
                ));
            }
            environment.insert("NORI_GEMINI_AUTH".into(), auth.into());
        }
        let node = which::which(options.get("node_path").unwrap_or("node"))
            .map_err(|error| not_found(format!("Node executable not found: {error}")))?;
        let module = options
            .get("core_path")
            .map(str::to_owned)
            .or_else(|| spec.env("NORI_GEMINI_CORE_MODULE"))
            .ok_or_else(|| {
                not_found(
                    "set NORI_GEMINI_CORE_MODULE or core_path to the official Gemini Core entry point"
                        .to_owned(),
                )
            })?;
        Ok(Self {
            node,
            module: PathBuf::from(module).canonicalize()?,
            environment,
        })
    }
}

struct Session {
    engine: Arc<Engine>,
    models: std::sync::Mutex<Options>,
    /// Held while a prompt is handed to the engine, so a cancel never reaches the engine ahead of
    /// the prompt it is meant for.
    prompt_start: tokio::sync::Mutex<()>,
    busy: AtomicBool,
}

struct Busy<'a>(&'a AtomicBool);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn model_options(ready: &Value) -> Result<Options, AgentError> {
    let current = ready["model"]
        .as_str()
        .ok_or_else(|| AgentError::internal("Gemini engine reported no model"))?;
    let mut models: Vec<&str> = ready["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    models.sort_unstable();
    models.dedup();
    let choices = models
        .into_iter()
        .map(|model| Choice::new(model, model))
        .collect();
    Ok(Options::new(vec![Select::model(choices, current)]))
}

#[async_trait]
impl Provider for Gemini {
    type Session = Session;

    fn profile(&self) -> Profile {
        Profile::new("nori-gemini-acp", "Gemini", env!("CARGO_PKG_VERSION")).capabilities(
            Capabilities {
                image: true,
                embedded_context: true,
                mcp_http: true,
                mcp_sse: true,
                additional_directories: true,
                ..Capabilities::default()
            },
        )
    }

    async fn open(&self, request: NewSessionRequest) -> Result<Opened<Session>, AgentError> {
        if !request.cwd.is_absolute()
            || request
                .additional_directories
                .iter()
                .any(|path| !path.is_absolute())
        {
            return Err(AgentError::invalid(
                "Gemini adapter requires absolute workspace directories",
            ));
        }
        let mut servers = Map::new();
        for endpoint in nori_acp_kit::mcp_endpoints(request.mcp_servers)? {
            let pairs = |pairs: Vec<(String, String)>| -> Map<String, Value> {
                pairs
                    .into_iter()
                    .map(|(key, value)| (key, value.into()))
                    .collect()
            };
            servers.insert(
                endpoint.name,
                match endpoint.transport {
                    McpTransport::Stdio { command, args, env } => {
                        json!({"command": command, "args": args, "env": pairs(env)})
                    }
                    McpTransport::Http { url, headers } => {
                        json!({"httpUrl": url, "headers": pairs(headers)})
                    }
                    McpTransport::Sse { url, headers } => {
                        json!({"url": url, "headers": pairs(headers)})
                    }
                },
            );
        }
        let id = uuid::Uuid::new_v4().to_string();
        let init = json!({"type": "init", "sessionId": id, "cwd": request.cwd, "mcpServers": servers,
            "additionalDirectories": request.additional_directories,
            "auth": crate::auth::select(&self.environment)?});
        let engine = Engine::start(
            &self.node,
            &self.module,
            &request.cwd,
            &self.environment,
            init,
        )
        .await
        .map_err(AgentError::from)?;
        let models = model_options(&engine.catalog)?;
        let options: Vec<SessionConfigOption> = models.to_acp();
        let session = Session {
            engine: Arc::new(engine),
            models: std::sync::Mutex::new(models),
            prompt_start: tokio::sync::Mutex::new(()),
            busy: AtomicBool::new(false),
        };
        Ok(Opened::new(SessionId::new(id), session).options(options))
    }

    async fn run_turn(
        &self,
        session: &Session,
        input: Vec<ContentBlock>,
        turn: &TurnCtx,
    ) -> Result<StopReason, AgentError> {
        let parts = prompt_parts(input)?;
        session.busy.store(true, Ordering::SeqCst);
        let _busy = Busy(&session.busy);
        let engine = &session.engine;
        {
            let _start = session.prompt_start.lock().await;
            if turn.is_cancelled() {
                return Ok(StopReason::Cancelled);
            }
            engine
                .send(&json!({"type": "prompt", "parts": parts}))
                .await?;
        }
        let mut ledger = ToolLedger::default();
        let mut cancel_sent = false;
        // The read is kept across loop turns: dropping a half-finished line read loses data.
        let mut receive = Box::pin(engine.receive());
        loop {
            let event = tokio::select! {
                event = &mut receive => {
                    receive = Box::pin(engine.receive());
                    event?
                }
                () = turn.cancelled(), if !cancel_sent => {
                    cancel_sent = true;
                    engine.send(&json!({"type": "cancel"})).await?;
                    continue;
                }
            };
            match event["type"].as_str() {
                Some("content") => {
                    turn.sink
                        .text(event["value"].as_str().unwrap_or_default())?;
                }
                Some("thought") => {
                    if let Some(text) = event["value"]["description"].as_str() {
                        turn.sink.thought(text)?;
                    }
                }
                Some("tool") => tool_event(&turn.sink, &mut ledger, &event)?,
                Some("permission") => {
                    tokio::spawn(answer_permission(
                        turn.sink.clone(),
                        Arc::clone(engine),
                        event,
                    ));
                }
                Some("done") => {
                    if let Some(usage) = event.get("usage") {
                        report_usage(&turn.sink, usage)?;
                    }
                    return Ok(match event["reason"].as_str() {
                        Some("cancelled") => StopReason::Cancelled,
                        Some("max_tokens") => StopReason::MaxTokens,
                        Some("max_turn_requests") => StopReason::MaxTurnRequests,
                        Some("refusal") => StopReason::Refusal,
                        _ => StopReason::EndTurn,
                    });
                }
                Some("error") => {
                    return Err(AgentError::provider(
                        None,
                        event["message"].as_str().unwrap_or("Gemini engine failed"),
                    ));
                }
                _ => return Err(AgentError::internal("Unexpected Gemini engine event")),
            }
        }
    }

    async fn configure(
        &self,
        session: &Session,
        id: &str,
        value: &str,
    ) -> Result<Vec<SessionConfigOption>, AgentError> {
        if id != nori_acp_kit::MODEL {
            return Err(AgentError::invalid("Unknown configuration option"));
        }
        if session.busy.load(Ordering::SeqCst) {
            return Err(AgentError::Busy(
                "Cannot change the model during a prompt".to_owned(),
            ));
        }
        let mut models = session
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let options = models.set(id, value)?;
        session
            .engine
            .send(&json!({"type": "set_model", "model": value}))
            .await?;
        let response = session.engine.receive().await?;
        if response["type"] != "configured" || response["model"] != value {
            return Err(AgentError::internal(
                "Gemini engine rejected model selection",
            ));
        }
        *session
            .models
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = models;
        Ok(options)
    }

    async fn close(&self, session: &Session) {
        session.engine.stop().await;
    }
}

/// The prompt as Core's content parts.
fn prompt_parts(input: Vec<ContentBlock>) -> Result<Vec<Value>, AgentError> {
    let mut parts = Vec::new();
    for block in input {
        match block {
            ContentBlock::Text(text) => {
                let hidden_from_model = text
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.audience.as_ref())
                    .is_some_and(|audience| {
                        !audience.contains(&nori_acp_kit::schema::Role::Assistant)
                    });
                if !hidden_from_model {
                    parts.push(json!({"text": text.text}));
                }
            }
            ContentBlock::Image(image) if !image.data.is_empty() => {
                parts
                    .push(json!({"inlineData": {"mimeType": image.mime_type, "data": image.data}}));
            }
            ContentBlock::Resource(resource) => match resource.resource {
                EmbeddedResourceResource::TextResourceContents(text) => {
                    parts.push(json!({"text": format!("[{}]\n{}", text.uri, text.text)}));
                }
                EmbeddedResourceResource::BlobResourceContents(blob)
                    if blob.mime_type.is_some() =>
                {
                    parts.push(
                        json!({"inlineData": {"mimeType": blob.mime_type, "data": blob.blob}}),
                    );
                }
                _ => return Err(AgentError::invalid("Unsupported embedded resource")),
            },
            _ => {
                return Err(AgentError::invalid(
                    "Gemini adapter cannot use this prompt content",
                ));
            }
        }
    }
    Ok(parts)
}

/// One of Core's tool events as the ACP tool call it belongs to.
fn tool_event(sink: &Sink, ledger: &mut ToolLedger, event: &Value) -> Result<(), AgentError> {
    let id = event["callId"].as_str().unwrap_or_default();
    let title = event["display"]["name"]
        .as_str()
        .or_else(|| event["name"].as_str())
        .unwrap_or("Tool");
    let call = ToolCall::new(id.to_owned(), title.to_owned()).raw_input(event["args"].clone());
    let updates = match event["status"].as_str().unwrap_or("pending") {
        "pending" => ledger
            .open(call.status(ToolCallStatus::Pending))
            .into_iter()
            .collect(),
        "executing" => ledger
            .progress(ToolCallUpdate::new(
                id.to_owned(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::InProgress)
                    .raw_output(event["result"].clone()),
            ))
            .into_iter()
            .collect(),
        status => {
            let content = crate::tool_output::content(&event["result"])
                .into_iter()
                .filter_map(|block| serde_json::from_value::<ToolCallContent>(block).ok())
                .collect();
            let outcome = if status == "success" {
                nori_acp_kit::Outcome::Completed
            } else {
                nori_acp_kit::Outcome::Failed
            };
            ledger.close(call, outcome, content, Some(event["result"].clone()), None)
        }
    };
    for update in updates {
        sink.update(update)?;
    }
    Ok(())
}

/// Ask the client about a tool Core wants to run, and give Core the answer.
async fn answer_permission(sink: Sink, engine: Arc<Engine>, event: Value) {
    let title = event["details"]["title"]
        .as_str()
        .or_else(|| event["name"].as_str())
        .unwrap_or("Gemini tool");
    let tool = ToolCallUpdate::new(
        event["callId"].as_str().unwrap_or_default().to_owned(),
        ToolCallUpdateFields::new().title(title.to_owned()),
    );
    let options = vec![
        PermissionOption::new("allow", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ];
    let allow = matches!(
        sink.ask_permission(tool, options).await,
        Ok(nori_acp_kit::PermissionDecision::Selected(option)) if option.to_string() == "allow"
    );
    let _ = engine
        .send(&json!({"type": "permission", "id": event["id"], "allow": allow}))
        .await;
}

/// Core's token counts for the prompt, as the contract's usage report.
fn report_usage(sink: &Sink, usage: &Value) -> Result<(), AgentError> {
    let count = |key: &str| usage[key].as_i64().unwrap_or(0);
    let cached = count("cached");
    let tokens = TokenBreakdown {
        input: (count("prompt") - cached).max(0),
        output: count("candidates"),
        cache_read: cached,
        cache_write: 0,
        reasoning: count("thoughts"),
    };
    let mut report = UsageReport::default().tokens(tokens);
    if let Some(model) = usage["model"].as_str() {
        report = report.model(model);
    }
    let context = usage["contextSize"].as_i64().map(|size| ContextUsage {
        used: count("lastPrompt"),
        size,
    });
    sink.usage(context, &report)?;
    Ok(())
}
