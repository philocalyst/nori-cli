//! Translate one OpenCode v2 execution into ordered ACP updates.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::StreamExt;
use nori_acp_kit::AgentError;
use nori_acp_kit::ContextUsage;
use nori_acp_kit::Cx;
use nori_acp_kit::Sink;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::Turn;
use nori_acp_kit::UsageReport;
use nori_acp_kit::acp;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::Usage;
use reqwest::Method;
use serde_json::Value;
use serde_json::json;

use crate::agent::Session;
use crate::children::Children;
use crate::children::Parent;
use crate::prompt::PromptInput;
use crate::provider_error;
use crate::server::failure;

type KnownChildren = Arc<tokio::sync::Mutex<HashMap<String, Arc<Session>>>>;
type ResponseTx = tokio::sync::oneshot::Sender<acp::Result<PromptResponse>>;

/// Token counts over the turn, and how full the context was after the last step.
#[derive(Default)]
struct StepUsage {
    total: TokenBreakdown,
    context_used: Option<i64>,
}

impl StepUsage {
    fn record(&mut self, tokens: &Value) {
        let step = TokenBreakdown {
            input: tokens["input"].as_i64().unwrap_or_default(),
            output: tokens["output"].as_i64().unwrap_or_default(),
            reasoning: tokens["reasoning"].as_i64().unwrap_or_default(),
            cache_read: tokens["cache"]["read"].as_i64().unwrap_or_default(),
            cache_write: tokens["cache"]["write"].as_i64().unwrap_or_default(),
        };
        self.total.add(&step);
        self.context_used = Some(step.total());
    }

    /// The response's own usage, with the totals the model reported.
    fn response_usage(&self) -> Usage {
        let total = &self.total;
        let optional = |count: i64| u64::try_from(count).ok();
        Usage::new(
            optional(total.total()).unwrap_or_default(),
            optional(total.input).unwrap_or_default(),
            optional(total.output).unwrap_or_default(),
        )
        .thought_tokens(optional(total.reasoning))
        .cached_read_tokens(optional(total.cache_read))
        .cached_write_tokens(optional(total.cache_write))
    }
}

pub(crate) async fn run(
    session: Arc<Session>,
    request: PromptRequest,
    client: Cx,
    known_children: KnownChildren,
    turn: Turn,
) -> acp::Result<PromptResponse> {
    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut response_tx = Some(response_tx);
        let result = run_with_children(
            session,
            request,
            client,
            &mut response_tx,
            known_children,
            turn,
        )
        .await;
        if let Some(response_tx) = response_tx {
            let _ = response_tx.send(result);
        }
    });
    response_rx.await.map_err(acp::Error::into_internal_error)?
}

/// Run the prompt. The response goes out as soon as the session's own execution ends, unless
/// children are still working; the task then keeps following them with the turn released.
async fn run_with_children(
    session: Arc<Session>,
    request: PromptRequest,
    client: Cx,
    response_tx: &mut Option<ResponseTx>,
    known_children: KnownChildren,
    turn: Turn,
) -> acp::Result<PromptResponse> {
    let cancel = turn.token();
    let mut turn = Some(turn);
    if let Some(mcp) = &session.mcp {
        crate::session_mcp::SessionMcp::prepare(mcp, &session.server).await?;
    }
    let input = PromptInput::from_blocks(request.prompt)?;
    let id = request.session_id.to_string();
    let mut events = session.server.events().await.map_err(failure)?;
    if !input.synthetic.is_empty() {
        session
            .server
            .request(
                Method::POST,
                &["session", &id, "synthetic"],
                Some(&json!({"text":input.synthetic.join("\n\n"),"description":"ACP embedded context","delivery":"steer","resume":false})),
            )
            .await
            .map_err(failure)?;
    }
    let command = input.slash_command();
    let known_command = match &command {
        Some((name, _)) => session
            .catalog
            .lock()
            .await
            .commands()
            .iter()
            .any(|item| item["name"] == *name),
        None => false,
    };
    let compact_id = command
        .as_ref()
        .is_some_and(|(name, _)| name == "compact")
        .then(|| format!("msg_{}", uuid::Uuid::new_v4().simple()));
    let files = &input.files;
    let (method, body) = match (&compact_id, command) {
        (Some(compact_id), _) => ("compact", json!({"id":compact_id})),
        (None, Some((name, args))) if known_command => (
            "command",
            json!({"name":name,"text":args,"files":files,"delivery":"steer"}),
        ),
        _ => (
            "prompt",
            json!({"text":input.text,"files":files,"delivery":"steer"}),
        ),
    };
    let prompt_path = ["session", &id, method];
    let result = tokio::select! {
        result = session.server.request(Method::POST, &prompt_path, Some(&body)) => result.map_err(failure)?,
        () = cancel.cancelled() => return Ok(PromptResponse::new(StopReason::Cancelled)),
    };
    let message_id = if let Some(id) = compact_id.as_deref() {
        Some(id)
    } else if known_command {
        None
    } else {
        Some(result["data"]["id"].as_str().ok_or_else(|| {
            AgentError::internal("OpenCode did not return the submitted message id")
        })?)
    };
    let sink = Sink::new(&client, request.session_id.clone());
    let mut started = false;
    let mut children = Children::default();
    let mut tools = HashMap::<(String, String), (String, Value)>::new();
    let mut terminal = None::<Result<StopReason, AgentError>>;
    let mut step_error = None::<Value>;
    let mut finish = None::<String>;
    let mut compactions = HashMap::<String, String>::new();
    let mut usage = StepUsage::default();
    let mut cancel_since = None::<std::time::Instant>;
    loop {
        let event = tokio::select! {
            event = events.next() => match event { Some(event) => event, None => break },
            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                if session.closed.load(Ordering::SeqCst) {
                    return Ok(PromptResponse::new(StopReason::Cancelled));
                }
                if response_tx.is_some() && cancel.is_cancelled() {
                    let since = cancel_since.get_or_insert_with(std::time::Instant::now);
                    if !started || since.elapsed() >= std::time::Duration::from_secs(5) {
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                }
                continue;
            }
        };
        let event = event.map_err(AgentError::internal)?;
        let value: Value = serde_json::from_str(&event.data).map_err(AgentError::from)?;
        let Some(kind) = value["type"].as_str() else {
            continue;
        };
        let data = &value["data"];
        let event_session = data["sessionID"]
            .as_str()
            .or_else(|| data["form"]["sessionID"].as_str())
            .unwrap_or_default();
        if kind == "session.created" {
            children
                .on_created(
                    &Parent {
                        session: &session,
                        id: &id,
                        response_pending: response_tx.is_some(),
                        known_children: &known_children,
                    },
                    &client,
                    event_session,
                    data,
                )
                .await?;
            continue;
        }
        if kind == "session.model.selected"
            && children.on_model_selected(&client, event_session, data)?
        {
            continue;
        }
        // Once the parent response has been sent, this subscriber only tracks
        // descendants already associated with that turn.
        if response_tx.is_none() && event_session == id {
            continue;
        }
        if event_session == id
            && kind == "session.inbox.delivered"
            && (message_id.is_none() || data["inboxID"].as_str() == message_id)
        {
            started = true;
        }
        if event_session == id
            && kind == "session.compaction.started"
            && data["inputID"].as_str() == message_id
            && compact_id.is_some()
        {
            started = true;
        }
        if !started {
            continue;
        }
        let is_child = children.identity(event_session).is_some();
        if event_session != id && !is_child {
            continue;
        }
        if event_session == id
            && kind == "session.retry.scheduled"
            && provider_error::is_exhausted(&data["error"])
        {
            let _ = session
                .server
                .request(
                    Method::POST,
                    &["session", &id, "interrupt"],
                    Some(&json!({})),
                )
                .await;
            if cancel.is_cancelled() {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            return Err(provider_error::failure(&data["error"]).into());
        }
        if event_session == id
            && matches!(
                kind,
                "session.compaction.started"
                    | "session.compaction.ended"
                    | "session.compaction.failed"
            )
        {
            let message_id = if kind == "session.compaction.started" {
                let message_id = data["inputID"]
                    .as_str()
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        value["id"]
                            .as_str()
                            .map(|id| id.replacen("evt_", "msg_", 1))
                    })
                    .unwrap_or_default();
                compactions.insert(id.clone(), message_id.clone());
                message_id
            } else {
                compactions.remove(&id).unwrap_or_default()
            };
            let status = match kind {
                "session.compaction.started" => "started",
                "session.compaction.ended" => "completed",
                _ => "failed",
            };
            client.send_notification(acp::UntypedMessage::new("session/update", json!({"sessionId":id,"update":{"sessionUpdate":"session_info_update","_meta":{"opencode/compaction":{"status":status,"messageId":message_id,"reason":data["reason"]}}}}))?)?;
            continue;
        }
        if kind == "permission.asked" {
            let session = session.clone();
            let client = client.clone();
            let client_session = request.session_id.clone();
            let event = data.clone();
            let child = children.identity(event_session);
            tokio::spawn(async move {
                crate::permission::ask_permission(session, client, client_session, event, child)
                    .await;
            });
            continue;
        }
        if kind == "form.created" {
            if data["form"]["metadata"]["kind"] == "question" {
                let session = session.clone();
                let client = client.clone();
                let form = data["form"].clone();
                tokio::spawn(async move {
                    crate::question::ask(session, client, form).await;
                });
                continue;
            }
            if let (Some(form_session), Some(form_id)) = (
                data["form"]["sessionID"].as_str(),
                data["form"]["id"].as_str(),
            ) {
                session
                    .server
                    .request(
                        Method::DELETE,
                        &["session", form_session, "form", form_id],
                        None,
                    )
                    .await
                    .map_err(failure)?;
            }
            continue;
        }
        if is_child {
            children.on_execution(&client, event_session, kind)?;
        } else {
            if matches!(kind, "session.step.ended" | "session.step.failed")
                && data["tokens"].is_object()
            {
                usage.record(&data["tokens"]);
            }
            match kind {
                "session.step.started" => step_error = None,
                "session.step.failed" => step_error = Some(data["error"].clone()),
                "session.step.ended" => finish = data["finish"].as_str().map(ToOwned::to_owned),
                "session.execution.succeeded" => {
                    terminal = Some(match &step_error {
                        Some(error) => provider_error::outcome(error),
                        None => Ok(match finish.as_deref() {
                            Some("length") => StopReason::MaxTokens,
                            Some("content-filter") => StopReason::Refusal,
                            _ => StopReason::EndTurn,
                        }),
                    });
                }
                "session.execution.interrupted" => terminal = Some(Ok(StopReason::Cancelled)),
                "session.execution.failed" => {
                    terminal = Some(provider_error::outcome(&data["error"]));
                }
                _ => {}
            }
        }
        let mut enriched = data.clone();
        if let Some(tool_id) = data["id"].as_str() {
            let key = (event_session.to_owned(), tool_id.to_owned());
            if kind == "session.tool.input.started" {
                tools.insert(
                    key.clone(),
                    (
                        data["name"].as_str().unwrap_or("Tool").to_owned(),
                        json!({}),
                    ),
                );
            }
            if let Some((name, input)) = tools.get_mut(&key) {
                if kind == "session.tool.called" {
                    *input = data["input"].clone();
                }
                enriched["name"] = name.clone().into();
                if kind == "session.tool.progress" {
                    enriched["input"] = input.clone();
                }
            }
            if matches!(kind, "session.tool.success" | "session.tool.failed") {
                tools.remove(&key);
            }
        }
        if let Some(update) = crate::event::update_for_event(kind, &enriched) {
            // A child's updates use its own session ID, allowing the host to route them
            // without guessing from titles, tool status, or private metadata.
            client.send_notification(acp::UntypedMessage::new(
                "session/update",
                json!({"sessionId":event_session,"update":update}),
            )?)?;
        }
        if let Some(result) = terminal.take() {
            let stop = if cancel.is_cancelled() {
                StopReason::Cancelled
            } else {
                result?
            };
            let mut response = PromptResponse::new(stop);
            if let Some(used) = usage.context_used {
                let (model, size) = {
                    let catalog = session.catalog.lock().await;
                    (catalog.current_model(), catalog.context_limit())
                };
                let mut report = UsageReport::default().tokens(usage.total);
                if let Some(model) = model {
                    report = report.model(model);
                }
                sink.usage(size.map(|size| ContextUsage { used, size }), &report)?;
                response = response.usage(usage.response_usage());
            }
            if !children.has_open() {
                return Ok(response);
            }
            drop(turn.take());
            if let Some(response_tx) = response_tx.take() {
                let _ = response_tx.send(Ok(response));
            }
        }
        if response_tx.is_none() && !children.has_open() {
            return Ok(PromptResponse::new(StopReason::EndTurn));
        }
    }
    Err(AgentError::internal("OpenCode event stream ended during a prompt").into())
}
