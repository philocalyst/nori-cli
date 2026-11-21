//! The agent loop: read what is waiting, ask the model, run the tools it calls, repeat until it
//! answers without calling any.
//!
//! Every step reads the session's dials afresh, so a model, effort or mode change made while a
//! turn runs is accepted at once and applies from the next model call. A prompt that arrives
//! while a turn runs is folded in at the next step (or keeps the turn going when it would have
//! ended). Cancellation stops the stream or the tool at once; whatever the model had said by
//! then is kept, and every tool call it made gets an answer, so the conversation stays valid.

use std::time::Duration;

use nori_acp_kit::AgentError;
use nori_acp_kit::ContextUsage;
use nori_acp_kit::FailureKind;
use nori_acp_kit::Sink;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::UsageReport;
use nori_acp_kit::schema::StopReason;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_llm::ApiError;
use nori_llm::Delta;
use nori_llm::ErrorKind;
use nori_llm::Message;
use nori_llm::Part;
use nori_llm::Reasoning;
use nori_llm::Request;
use nori_llm::Response;
use nori_llm::Stop;
use tokio_util::sync::CancellationToken;

use crate::flavor::Connection;
use crate::flavor::EffortStyle;
use crate::flavor::Flavor;
use crate::session::Session;

/// Model calls one turn may make before it stops to ask the person.
const MAX_STEPS: i64 = 200;
/// Attempts at one model call that fails in a way waiting can fix.
const MAX_RETRIES: i64 = 4;
/// The longest a provider's `retry-after` is waited out inside a turn.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(60);
/// What one answer may be at most when the model does not say.
const DEFAULT_MAX_OUTPUT: i64 = 32_000;

/// What a turn runs with.
pub(crate) struct TurnEnv<'a, F: Flavor> {
    pub(crate) flavor: &'a F,
    pub(crate) client: &'a nori_llm::Client,
    pub(crate) session: &'a Session,
    pub(crate) sink: &'a Sink,
    pub(crate) cancel: CancellationToken,
}

/// Run the turn for the prompt with `ticket`. When another loop already read that prompt (it
/// was steered into a running turn), there is nothing left to do.
pub(crate) async fn run<F: Flavor>(
    env: &TurnEnv<'_, F>,
    ticket: i64,
) -> Result<StopReason, AgentError> {
    let session = env.session;
    let mut conversation = session.conversation.lock().await;
    if !session.waiting(ticket) {
        return Ok(if env.cancel.is_cancelled() {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        });
    }
    let mut steps = 0;
    loop {
        for message in session.take_waiting() {
            session.journal.message(&message).await?;
            conversation.push(message);
        }
        if env.cancel.is_cancelled() {
            return Ok(StopReason::Cancelled);
        }
        steps += 1;
        if steps > MAX_STEPS {
            return Ok(StopReason::MaxTurnRequests);
        }
        let dials = session.dials();
        let workspace = session.workspace();
        let info = workspace
            .model(&dials.model)
            .cloned()
            .ok_or_else(|| AgentError::invalid(format!("unknown model {}", dials.model)))?;
        let rules = session.rules(&dials);
        let mode = session.mode(&dials);
        let tools: Vec<nori_llm::ToolSpec> = session
            .tool_list()
            .iter()
            .filter(|tool| tool.offered(&info))
            .map(|tool| tool.spec())
            .filter(|spec| {
                !crate::rules::denied_everywhere(&rules, &spec.name)
                    && !crate::rules::denied_everywhere(
                        &rules,
                        crate::rules::primary_action(&spec.name),
                    )
            })
            .collect();
        let system = crate::prompt::system(
            env.flavor.system_prompt(&info),
            mode.as_ref(),
            &workspace,
            &session.cwd,
            session
                .host
                .as_ref()
                .and_then(nori_acp_kit::HostBridge::instructions),
        );
        let max_output = info
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT)
            .min(DEFAULT_MAX_OUTPUT * 2);
        // A credential the provider refuses before answering is renewed once (another program
        // may have refreshed it on disk) and the request sent again.
        let mut renewed = false;
        let (connection, called) = loop {
            let mut connection = env.flavor.connect(&session.cwd, &dials.model).await?;
            let named = env
                .flavor
                .session_headers(&session.id, &connection.endpoint);
            connection.endpoint.headers.extend(
                named
                    .into_iter()
                    .map(|(name, value)| (name, nori_llm::Secret::new(value))),
            );
            let reasoning =
                dials
                    .effort
                    .as_deref()
                    .and_then(|effort| match connection.effort_style {
                        EffortStyle::Named => {
                            (effort != "none").then(|| Reasoning::Effort(effort.to_owned()))
                        }
                        EffortStyle::AnthropicBudget => {
                            anthropic_budget(effort).map(Reasoning::Budget)
                        }
                    });
            let mut extra_body = connection.extra_body.clone();
            extra_body.extend(dials.body_fields(&workspace));
            let request = Request {
                model: connection.model.clone(),
                origin: dials.model.clone(),
                system: system.clone(),
                messages: conversation.clone(),
                tools: tools.clone(),
                max_output_tokens: connection.output_limit.then_some(max_output),
                reasoning,
                temperature: connection.temperature,
                reasoning_echo_field: connection.reasoning_echo_field.clone(),
                cache_key: Some(session.id.clone()),
                extra_body,
            };
            let called = call(env, &connection, &request).await;
            if !renewed
                && matches!(called, Called::Failed(AgentError::AuthRequired(_), None))
                && env.flavor.reauthorize().await
            {
                renewed = true;
                continue;
            }
            break (connection, called);
        };
        let response = match called {
            Called::Done(response) => response,
            Called::Cancelled(partial) => {
                keep_partial(session, &mut conversation, partial).await;
                keep_waiting(session, &mut conversation).await;
                return Ok(StopReason::Cancelled);
            }
            Called::Refused(partial) => {
                keep_partial(session, &mut conversation, partial).await;
                return Ok(StopReason::Refusal);
            }
            Called::Failed(error, partial) => {
                keep_partial(session, &mut conversation, partial).await;
                return Err(error);
            }
        };
        report_usage(
            env,
            &connection,
            &dials.model,
            info.context_window,
            &response,
        );
        session.journal.message(&response.message).await?;
        conversation.push(response.message.clone());
        let calls: Vec<nori_llm::ToolCall> = response.message.tool_calls().cloned().collect();
        if calls.is_empty() {
            if session.has_waiting() {
                continue;
            }
            return Ok(match response.stop {
                Stop::MaxTokens => StopReason::MaxTokens,
                Stop::Refusal => StopReason::Refusal,
                Stop::EndTurn | Stop::ToolUse | Stop::Other(_) => StopReason::EndTurn,
            });
        }
        let acted = crate::act::run_calls(env, &rules, calls).await;
        let results = Message::user(acted.results);
        session.journal.message(&results).await?;
        conversation.push(results);
        if env.cancel.is_cancelled() {
            keep_waiting(session, &mut conversation).await;
            return Ok(StopReason::Cancelled);
        }
        if acted.rejected {
            return Ok(StopReason::EndTurn);
        }
    }
}

/// An Anthropic thinking budget for an effort name.
fn anthropic_budget(effort: &str) -> Option<i64> {
    match effort {
        "none" | "off" => None,
        "minimal" => Some(1024),
        "low" => Some(4_000),
        "medium" => Some(10_000),
        "high" => Some(24_000),
        _ => Some(32_000),
    }
}

async fn keep_partial(
    session: &Session,
    conversation: &mut Vec<Message>,
    partial: Option<Message>,
) {
    if let Some(message) = partial {
        if let Err(error) = session.journal.message(&message).await {
            tracing::warn!(%error, "could not record an interrupted answer");
        }
        conversation.push(message);
    }
}

/// Prompts sent into a turn that was cancelled before reading them are still what the person
/// said: they join the conversation instead of surfacing in a later turn.
async fn keep_waiting(session: &Session, conversation: &mut Vec<Message>) {
    for message in session.take_waiting() {
        if let Err(error) = session.journal.message(&message).await {
            tracing::warn!(%error, "could not record a prompt");
        }
        conversation.push(message);
    }
}

enum Called {
    Done(Response),
    /// Cancelled, with what the model had written.
    Cancelled(Option<Message>),
    /// A content filter stopped it.
    Refused(Option<Message>),
    Failed(AgentError, Option<Message>),
}

/// What streamed before a call ended early.
#[derive(Default)]
struct Streamed {
    text: String,
    reasoning: String,
    started: Vec<String>,
}

impl Streamed {
    fn emitted(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty() || !self.started.is_empty()
    }

    /// The answer so far, without tool calls (their arguments are incomplete).
    fn partial(&self) -> Option<Message> {
        (!self.text.is_empty()).then(|| Message::assistant(vec![Part::text(self.text.clone())]))
    }

    /// Close the cards of tool calls that will never run.
    fn abandon(&self, sink: &Sink, why: &str) {
        for id in &self.started {
            let _ = sink.tool_update(ToolCallUpdate::new(
                id.clone(),
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![why.to_owned().into()]),
            ));
        }
    }
}

async fn call<F: Flavor>(
    env: &TurnEnv<'_, F>,
    connection: &Connection,
    request: &Request,
) -> Called {
    let mut attempt = 0;
    loop {
        let mut streamed = Streamed::default();
        let sink = env.sink;
        let mut on_delta = |delta: Delta| match delta {
            Delta::Text(text) => {
                streamed.text.push_str(&text);
                let _ = sink.text(text);
            }
            Delta::Reasoning(text) => {
                streamed.reasoning.push_str(&text);
                let _ = sink.thought(text);
            }
            Delta::ToolCallStarted { id, name } => {
                streamed.started.push(id.clone());
                let _ = sink.tool_call(ToolCall::new(id, name).status(ToolCallStatus::Pending));
            }
            Delta::ToolCallInput { .. } => {}
        };
        let outcome = tokio::select! {
            outcome = env.client.stream(&connection.endpoint, request, &mut on_delta) => Some(outcome),
            () = env.cancel.cancelled() => None,
        };
        let error = match outcome {
            None => {
                streamed.abandon(env.sink, "Cancelled.");
                return Called::Cancelled(streamed.partial());
            }
            Some(Ok(response)) => return Called::Done(response),
            Some(Err(error)) => error,
        };
        if error.kind == ErrorKind::ContentFilter {
            streamed.abandon(env.sink, &error.message);
            return Called::Refused(streamed.partial());
        }
        if error.is_retryable() && !streamed.emitted() && attempt < MAX_RETRIES {
            attempt += 1;
            let wait = error
                .retry_after
                .map(|seconds| Duration::from_secs(u64::try_from(seconds.max(1)).unwrap_or(1)))
                .unwrap_or_else(|| Duration::from_secs(1 << attempt))
                .min(MAX_RETRY_WAIT);
            tracing::info!(
                attempt,
                wait_seconds = wait.as_secs(),
                error = %error,
                "retrying a model call"
            );
            tokio::select! {
                () = tokio::time::sleep(wait) => continue,
                () = env.cancel.cancelled() => return Called::Cancelled(None),
            }
        }
        streamed.abandon(env.sink, &error.message);
        return Called::Failed(agent_error(error), streamed.partial());
    }
}

/// A provider failure as the error the host classifies.
pub(crate) fn agent_error(error: ApiError) -> AgentError {
    let kind = match error.kind {
        ErrorKind::Auth => return AgentError::AuthRequired(error.message),
        ErrorKind::RateLimited => FailureKind::RateLimited,
        ErrorKind::QuotaExhausted => FailureKind::QuotaExhausted,
        ErrorKind::Overloaded => FailureKind::Capacity,
        ErrorKind::Server => FailureKind::ServerError,
        ErrorKind::PromptTooLong => FailureKind::PromptTooLong,
        ErrorKind::ModelNotFound => FailureKind::ModelUnavailable,
        ErrorKind::Network | ErrorKind::Timeout => FailureKind::Network,
        ErrorKind::InvalidRequest | ErrorKind::ContentFilter | ErrorKind::Decode => {
            return AgentError::Internal(error.message);
        }
    };
    AgentError::Provider {
        kind,
        detail: error.message,
        resets_at: None,
        retry_after: error.retry_after,
    }
}

fn report_usage<F: Flavor>(
    env: &TurnEnv<'_, F>,
    connection: &Connection,
    model: &str,
    context_window: Option<i64>,
    response: &Response,
) {
    let usage = response.usage;
    let mut report = UsageReport::default().model(model).tokens(TokenBreakdown {
        input: usage.input,
        output: usage.output,
        cache_read: usage.cache_read,
        cache_write: usage.cache_write,
        reasoning: usage.reasoning,
    });
    if let Some(account) = &connection.account {
        report = report.account(account.clone());
    }
    for window in env.flavor.rate_limits(&response.headers) {
        report = report.rate_limit(window);
    }
    let context = context_window.map(|size| ContextUsage {
        used: usage.context_tokens(),
        size,
    });
    if let Err(error) = env.sink.usage(context, &report) {
        tracing::debug!(%error, "could not report usage");
    }
}
