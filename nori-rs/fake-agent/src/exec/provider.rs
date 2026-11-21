//! The provider behind an agent: the limits it enforces on a prompt, in its own words.

use nori_acp_kit::schema::ContentBlock;
use serde_json::json;

use crate::world::Limits;
use crate::world::Provider;

/// The bytes of all the text blocks of a prompt, and of the longest of them.
pub(super) fn weigh(blocks: &[ContentBlock]) -> (i64, i64) {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(i64::try_from(text.text.len()).unwrap_or(i64::MAX)),
            _ => None,
        })
        .fold((0, 0), |(total, largest), size| {
            (total.saturating_add(size), largest.max(size))
        })
}

/// What the provider answers when the prompt is over a limit: the body of its error, as the
/// agent passes it on. `None` when the prompt fits.
pub(super) fn refusal(
    limits: &Limits,
    blocks: &[ContentBlock],
    bytes: i64,
    largest: i64,
) -> Option<String> {
    let body = if let Some(max) = limits.max_text_block_bytes
        && largest > max
    {
        // Which block went over: the first one that did.
        let index = blocks
            .iter()
            .position(|block| {
                matches!(block, ContentBlock::Text(text)
                    if i64::try_from(text.text.len()).unwrap_or(i64::MAX) > max)
            })
            .unwrap_or(0);
        match limits.provider {
            Provider::Openai => {
                let param = format!("input[{index}].content[0].text");
                json!({"type": "error", "error": {
                    "type": "invalid_request_error",
                    "code": "string_above_max_length",
                    "message": format!(
                        "Invalid '{param}': string too long. Expected a string with maximum \
                         length {max}, but got a string with length {largest} instead."
                    ),
                    "param": param,
                }})
            }
            Provider::Anthropic => json!({"type": "error", "error": {
                "type": "request_too_large",
                "message": "Request exceeds the maximum allowed number of bytes.",
            }}),
        }
    } else if let Some(max) = limits.max_prompt_bytes
        && bytes > max
    {
        let (tokens, max_tokens) = (bytes / 4, max / 4);
        match limits.provider {
            Provider::Openai => json!({"error": {
                "message": format!(
                    "This model's maximum context length is {max_tokens} tokens. However, your \
                     messages resulted in {tokens} tokens. Please reduce the length of the messages."
                ),
                "type": "invalid_request_error",
                "param": "messages",
                "code": "context_length_exceeded",
            }}),
            Provider::Anthropic => json!({"type": "error", "error": {
                "type": "invalid_request_error",
                "message": format!("prompt is too long: {tokens} tokens > {max_tokens} maximum"),
            }}),
        }
    } else {
        return None;
    };
    Some(format!(
        "{}{body}",
        limits.prefix.as_deref().unwrap_or_default()
    ))
}
