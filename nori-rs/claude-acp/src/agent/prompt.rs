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
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
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
/// How long a cancelled turn waits for the CLI to acknowledge its interrupt.
const INTERRUPT_WAIT: Duration = Duration::from_secs(2);

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
    // Cancelled while it waited for the turn before it: it never reaches the CLI.
    if turn.is_cancelled() {
        return Ok(PromptResponse::new(StopReason::Cancelled));
    }
    session.set_converter_request_id(request_id.clone()).await;

    // Everything that must happen before the first query: usually already done, because it
    // starts as soon as the session exists. A prompt that arrives first waits for that work.
    prepare_session(&session, cx, sessions.client_terminal()).await?;

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
    session.apply_pending_fast_mode().await;

    let client = session.client().await;
    let mut stream = client.receive_response();
    let mut interrupted_at: Option<Instant> = None;
    let mut last_result: Option<ResultMessage> = None;
    let mut quota_error: Option<AgentError> = None;
    // Why the model last stopped writing at the top level (`end_turn`, `max_tokens`, `refusal`).
    let mut model_stop: Option<String> = None;
    // How the CLI ended, when it ended before the turn did.
    let mut cli_exit: Option<ClaudeError> = None;
    let started = Instant::now();

    loop {
        // A cancelled turn (the client's cancel, an MCP cancellation, a newer prompt): interrupt
        // the CLI once, keep delivering what it still sends, and give up after a grace period.
        if turn.is_cancelled() {
            match interrupted_at {
                None => {
                    // Bounded: a CLI that is slow to acknowledge must not hold up the messages it
                    // is still sending, nor the end of the turn.
                    match tokio::time::timeout(INTERRUPT_WAIT, client.interrupt()).await {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            tracing::warn!(%session_id, %error, "Failed to interrupt the CLI");
                        }
                        Err(_) => {
                            tracing::warn!(%session_id, "The CLI did not acknowledge the interrupt");
                        }
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
            // The CLI is gone: nothing more will come, and its replacement starts with the next
            // query, resuming the conversation.
            Some(Err(error @ ClaudeError::CliExited(_))) => {
                cli_exit = Some(error);
                break;
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
                if let Some(stop) = &assistant.message.stop_reason {
                    model_stop = Some(stop.clone());
                }
            }
            Message::StreamEvent(event) if event.parent_tool_use_id.is_none() => {
                if let Some(stop) = event
                    .event
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .filter(|_| event.event["type"] == "message_delta")
                {
                    model_stop = Some(stop.to_owned());
                }
            }
            // A message kind newer than this adapter (`tool_progress`, say) reaches the client
            // whole, for a client that knows it, and never breaks the turn.
            Message::Unrecognized(raw_event) => {
                let meta = serde_json::Map::from_iter([(
                    "_claude/rawEvent".to_owned(),
                    raw_event.clone(),
                )]);
                if let Err(error) = sink.update(SessionUpdate::SessionInfoUpdate(
                    SessionInfoUpdate::new().meta(meta),
                )) {
                    tracing::warn!(%session_id, %error, "Unknown Claude event not forwarded");
                }
                continue;
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
    if cli_exit.is_some() {
        // The tools the CLI was running when it went away will never finish: end them, so the
        // client is not left with a call that spins forever.
        let unfinished = session.converter().await.unfinished_tool_uses();
        for tool_use_id in unfinished {
            let update = ToolCallUpdate::new(
                tool_use_id,
                ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Failed)
                    .content(vec![
                        ContentBlock::from(
                            "Claude Code stopped before this tool finished.".to_owned(),
                        )
                        .into(),
                    ]),
            );
            if let Err(error) = sink.tool_update(update) {
                tracing::warn!(%session_id, %error, "A cut-short tool was not ended");
            }
        }
    }

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
    if let Some(exit) = cli_exit {
        return Err(crate::types::from_sdk(exit));
    }
    let Some(result) = last_result else {
        return Err(AgentError::internal(
            "Claude Code ended the turn without a result",
        ));
    };
    if result.is_error {
        // What the CLI reports is usually the provider's own failure (`API Error: 529 {...}`):
        // the person reads the provider's sentence, and the class comes from its body.
        let detail = result
            .result
            .clone()
            .filter(|detail| !detail.trim().is_empty())
            .unwrap_or_else(|| format!("Claude Code ended the turn with {}", result.subtype));
        return Err(AgentError::provider(None, detail));
    }
    Ok(PromptResponse::new(stop_reason(
        &result.subtype,
        model_stop.as_deref(),
    )))
}

/// Why a turn that ended without an error ended: the model's own stop reason where it says the
/// answer was cut off (`max_tokens`) or declined (`refusal`), else what the CLI's result says.
fn stop_reason(subtype: &str, model_stop: Option<&str>) -> StopReason {
    match model_stop {
        Some("max_tokens" | "model_context_window_exceeded") => return StopReason::MaxTokens,
        Some("refusal") => return StopReason::Refusal,
        _ => {}
    }
    match subtype {
        "error_max_budget_usd" | "error_max_turns" | "error_max_structured_output_retries" => {
            StopReason::MaxTurnRequests
        }
        // A success, an execution cut short without an error, and a result this version does
        // not know all end the turn normally: none of them is the model declining.
        _ => StopReason::EndTurn,
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_models_own_stop_reason_says_when_an_answer_was_cut_off_or_declined() {
        let cases = [
            ("success", Some("max_tokens"), StopReason::MaxTokens),
            ("success", Some("refusal"), StopReason::Refusal),
            ("success", Some("end_turn"), StopReason::EndTurn),
            ("success", None, StopReason::EndTurn),
            ("error_max_turns", None, StopReason::MaxTurnRequests),
            // A result kind this version does not know is not the model declining.
            ("success_with_a_new_name", None, StopReason::EndTurn),
        ];
        for (subtype, model_stop, expected) in cases {
            assert_eq!(
                stop_reason(subtype, model_stop),
                expected,
                "{subtype} {model_stop:?}"
            );
        }
    }
}
