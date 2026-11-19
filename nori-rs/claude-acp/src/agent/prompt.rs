//! Running one prompt: send it to the CLI, stream its messages out as ACP updates, report usage.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use claude_code_agent_sdk::ClaudeError;
use claude_code_agent_sdk::Message;
use claude_code_agent_sdk::ResultMessage;
use claude_code_agent_sdk::UserContentBlock;
use futures::Stream;
use futures::StreamExt;
use nori_acp_kit::Cx;
use nori_acp_kit::FailureKind;
use nori_acp_kit::Sink;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::EmbeddedResourceResource;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::SessionInfoUpdate;
use nori_acp_kit::schema::SessionUpdate;
use nori_acp_kit::schema::StopReason;
use serde_json::Value;

use super::lifecycle::prepare_session;
use super::slash_commands::transform_mcp_command_input;
use crate::session::Session;
use crate::session::SessionManager;
use crate::types::AgentError;
use crate::types::Result;

type MessageStream<'a> =
    Pin<Box<dyn Stream<Item = std::result::Result<Message, ClaudeError>> + Send + 'a>>;

/// How long a cancelled turn may keep delivering the CLI's last messages before it stops waiting.
const CANCEL_GRACE: Duration = Duration::from_secs(5);

/// Run `request` to its end. A newer prompt, or a cancel, cancels the turn; the CLI is
/// interrupted once and the turn ends `Cancelled` when its last messages are in.
pub(super) async fn run(
    request: PromptRequest,
    sessions: &Arc<SessionManager>,
    cx: &Cx,
) -> Result<PromptResponse> {
    let session = sessions.get_session_or_error(request.session_id.0.as_ref())?;
    let session_id = session.session_id.clone();
    let sink = Sink::new(cx, request.session_id.clone());

    let request_id = request
        .meta
        .as_ref()
        .and_then(|meta| meta.get("request_id"))
        .and_then(Value::as_str)
        .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);

    let turn = session.begin_turn().await?;
    session.set_converter_request_id(request_id.clone()).await;

    // Everything that must happen before the first query: usually already done, because it
    // starts as soon as the session exists. A prompt that arrives first waits for that work.
    prepare_session(&session, cx).await?;

    {
        let mut client = session.client_mut().await;
        if request
            .prompt
            .iter()
            .any(|block| matches!(block, ContentBlock::Image(_)))
        {
            // A prompt with images goes as content blocks; slash commands are text only.
            client
                .query_with_content(user_blocks(&request.prompt))
                .await
                .map_err(crate::types::from_sdk)?;
        } else {
            let text = request
                .prompt
                .iter()
                .filter_map(block_text)
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                // Transform MCP command format: /mcp:server:cmd -> /server:cmd (MCP)
                client
                    .query(&transform_mcp_command_input(&text))
                    .await
                    .map_err(crate::types::from_sdk)?;
            }
        }
    }
    // A model chosen since the last prompt goes to the CLI now that a query exists.
    session.apply_pending_model().await;

    let client = session.client().await;
    let mut stream = client.receive_response();
    let mut interrupted_at: Option<Instant> = None;
    let mut last_result: Option<ResultMessage> = None;
    let mut quota_error: Option<AgentError> = None;
    let started = Instant::now();

    loop {
        // A cancelled turn (the client's cancel, an MCP cancellation, a newer prompt): interrupt
        // the CLI once, keep delivering what it still sends, and give up after a grace period.
        if turn.is_cancelled() {
            match interrupted_at {
                None => {
                    if let Err(error) = client.interrupt().await {
                        tracing::warn!(%session_id, %error, "Failed to interrupt the CLI");
                    }
                    interrupted_at = Some(Instant::now());
                }
                Some(at) if at.elapsed() > CANCEL_GRACE => {
                    tracing::warn!(%session_id, "The CLI kept talking after an interrupt");
                    drain(&session_id, &mut stream).await;
                    break;
                }
                Some(_) => {}
            }
        }

        // A timeout keeps the loop checking for cancellation while the stream is quiet.
        let next = match tokio::time::timeout(Duration::from_secs(1), stream.next()).await {
            Ok(next) => next,
            Err(_) => continue,
        };
        let message = match next {
            Some(Ok(message)) => message,
            None => break,
            Some(Err(error)) if error.to_string().contains("unknown variant") => {
                // Preserve the raw payload even when a future CLI message type is not known to
                // this SDK version.
                if let ClaudeError::MessageParse(parse_error) = &error
                    && let Some(raw_event) = parse_error.data.clone()
                {
                    let meta =
                        serde_json::Map::from_iter([("_claude/rawEvent".to_owned(), raw_event)]);
                    if let Err(error) = sink.update(SessionUpdate::SessionInfoUpdate(
                        SessionInfoUpdate::new().meta(meta),
                    )) {
                        tracing::warn!(%session_id, %error, "Unknown Claude event not preserved");
                    }
                }
                tracing::warn!(%session_id, %error, "Skipping a message type the SDK does not recognise");
                continue;
            }
            // An individual bad message does not fail the turn.
            Some(Err(error)) => {
                tracing::error!(%session_id, %error, "Error receiving message from Claude CLI");
                continue;
            }
        };

        match &message {
            Message::Result(result) => {
                tracing::info!(
                    %session_id,
                    subtype = %result.subtype,
                    is_error = result.is_error,
                    duration_ms = result.duration_ms,
                    num_turns = result.num_turns,
                    "Received ResultMessage from Claude CLI"
                );
                if let Some(usage) = &result.usage {
                    session
                        .usage()
                        .record_query(usage, result.model_usage.as_ref());
                }
                last_result = Some(result.clone());
            }
            Message::Assistant(assistant) if assistant.parent_tool_use_id.is_none() => {
                if let Some(model) = &assistant.message.model {
                    session.usage().set_model(model);
                }
                if let Some(usage) = &assistant.message.usage {
                    session.usage().record_context(usage);
                }
            }
            Message::RateLimitEvent(event) => {
                if session.usage().record_rate_limit(event) {
                    report_usage(&session, &sink);
                }
                quota_error = quota_error.or_else(|| subscription_quota_error(event));
            }
            _ => {}
        }

        // A cancelled turn ends with an error result the user already knows about, so it must
        // not surface as an error message in the transcript. Rate-limit events are reported as
        // usage above, not as transcript content.
        let cancelled_result =
            turn.is_cancelled() && matches!(&message, Message::Result(r) if r.is_error);
        if cancelled_result || matches!(message, Message::RateLimitEvent(_)) {
            continue;
        }
        let notifications = session
            .converter()
            .await
            .convert_message(&message, &session_id);
        for notification in notifications {
            if let Err(error) = cx.send_notification(notification) {
                tracing::warn!(%session_id, %error, "Failed to send notification");
            }
        }
    }

    tracing::info!(%session_id, elapsed_ms = started.elapsed().as_millis(), "Prompt completed");
    report_usage(&session, &sink);

    // The connection delivers notifications asynchronously, so a response sent now could
    // overtake the last updates. A short pause keeps them ahead of the end of the turn.
    tokio::time::sleep(Duration::from_millis(5)).await;
    session.clear_converter_request_id().await;
    session.clear_converter_cache().await;

    if turn.is_cancelled() {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }
    if let Some(error) = quota_error {
        return Err(error);
    }
    let Some(result) = last_result else {
        return Err(AgentError::internal("Session did not end in result"));
    };
    if result.is_error {
        let detail = result
            .result
            .clone()
            .unwrap_or_else(|| result.subtype.clone());
        return Err(AgentError::provider(
            None,
            format!("Query failed: {detail} (subtype: {})", result.subtype),
        ));
    }
    Ok(PromptResponse::new(stop_reason(&result.subtype)))
}

fn stop_reason(subtype: &str) -> StopReason {
    match subtype {
        // An execution cut short without an error ends the turn like a success.
        "success" | "error_during_execution" => StopReason::EndTurn,
        "error_max_budget_usd" | "error_max_turns" | "error_max_structured_output_retries" => {
            StopReason::MaxTurnRequests
        }
        _ => StopReason::Refusal,
    }
}

/// Send what the session has used so far.
fn report_usage(session: &Session, sink: &Sink) {
    let (context, report) = session.usage().report();
    if let Err(error) = sink.usage(context, &report) {
        tracing::warn!(session_id = %session.session_id, %error, "Usage not sent");
    }
}

/// A quota error, only for an explicit rejection of a known subscription usage window. Generic
/// `rate_limit_error` events remain transient.
fn subscription_quota_error(event: &Value) -> Option<AgentError> {
    let info = event.get("rate_limit_info")?;
    let window = info.get("rateLimitType")?.as_str()?;
    if info.get("status")?.as_str()? != "rejected"
        || !matches!(
            window,
            "five_hour" | "seven_day" | "seven_day_opus" | "seven_day_sonnet"
        )
    {
        return None;
    }
    Some(AgentError::Provider {
        kind: FailureKind::QuotaExhausted,
        detail: format!("Claude subscription usage limit reached ({window} window)"),
        resets_at: info
            .get("resetsAt")
            .and_then(Value::as_i64)
            .map(nori_acp_kit::rfc3339_utc),
        retry_after: None,
    })
}

/// The text a block contributes to the prompt, with resources as context tags. Images are not
/// text; they reach the CLI as image blocks (see [`user_blocks`]).
fn block_text(block: &ContentBlock) -> Option<String> {
    match block {
        ContentBlock::Text(text) => Some(text.text.clone()),
        ContentBlock::Resource(embedded) => match &embedded.resource {
            EmbeddedResourceResource::TextResourceContents(text) => Some(format!(
                "<context uri=\"{}\">\n{}\n</context>",
                text.uri, text.text
            )),
            EmbeddedResourceResource::BlobResourceContents(blob) => {
                Some(format!("<context uri=\"{}\" />", blob.uri))
            }
            _ => None,
        },
        ContentBlock::ResourceLink(link) => {
            let title = link.title.as_deref().unwrap_or("");
            Some(if title.is_empty() {
                format!("<resource uri=\"{}\" />", link.uri)
            } else {
                format!("[{title}]({})", link.uri)
            })
        }
        ContentBlock::Image(_) | ContentBlock::Audio(_) => None,
        _ => None,
    }
}

/// The prompt as the SDK's content blocks, in order. Audio is not supported by Claude Code.
fn user_blocks(prompt: &[ContentBlock]) -> Vec<UserContentBlock> {
    prompt
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Image(image) => {
                UserContentBlock::image_base64(image.mime_type.clone(), image.data.clone()).ok()
            }
            other => block_text(other).map(UserContentBlock::text),
        })
        .collect()
}

/// Drain the stream until it has been silent for 50ms (at most 5s), so leftover messages of a
/// cancelled query cannot leak into the next prompt.
async fn drain(session_id: &str, stream: &mut MessageStream<'_>) {
    let started = Instant::now();
    let mut last_message = started;
    while started.elapsed() < Duration::from_secs(5) {
        match tokio::time::timeout(Duration::from_millis(10), stream.next()).await {
            Ok(Some(_)) => last_message = Instant::now(),
            Ok(None) => return,
            Err(_) if last_message.elapsed() >= Duration::from_millis(50) => return,
            Err(_) => {}
        }
    }
    tracing::warn!(%session_id, "Drain reached its maximum duration while messages kept arriving");
}
