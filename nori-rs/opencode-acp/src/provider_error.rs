//! OpenCode v2 provider errors as the errors the host categorizes.

use nori_acp_kit::AgentError;
use nori_acp_kit::FailureKind;
use serde_json::Value;

/// OpenCode's error with the provider's own response body decoded under `error`, when the body is
/// JSON.
fn decoded(error: &Value) -> Value {
    let mut result = error.clone();
    let Some(body) = error
        .pointer("/response/body")
        .or_else(|| error.get("bodyString"))
        .and_then(Value::as_str)
    else {
        return result;
    };
    let Ok(body) = serde_json::from_str::<Value>(body) else {
        return result;
    };
    let provider = body
        .get("error")
        .filter(|value| value.is_object())
        .unwrap_or(&body);
    if provider.is_object()
        && let Some(object) = result.as_object_mut()
    {
        object.entry("error").or_insert_with(|| provider.clone());
    }
    result
}

/// A specific provider error code can signal exhaustion before OpenCode finishes retrying.
/// Only structured identifiers count: prose that merely mentions a quota does not.
pub(crate) fn is_exhausted(error: &Value) -> bool {
    let data = decoded(error);
    [Some(&data), data.get("error")]
        .into_iter()
        .flatten()
        .any(|value| {
            ["code", "type"].into_iter().any(|field| {
                matches!(
                    value.get(field).and_then(Value::as_str),
                    Some(
                        "insufficient_quota"
                            | "usage_limit_exceeded"
                            | "usage_limit_reached"
                            | "billing_hard_limit_reached"
                            | "subscription_exhausted"
                    )
                )
            })
        })
}

/// The failure an OpenCode step or execution reported, classified for the host's retry and
/// fallback policy.
pub(crate) fn failure(error: &Value) -> AgentError {
    let data = decoded(error);
    let detail = [
        data.pointer("/error/message"),
        data.get("message"),
        data.get("type"),
    ]
    .into_iter()
    .flatten()
    .find_map(Value::as_str)
    .unwrap_or("OpenCode reported an unspecified error")
    .to_owned();
    let kind = if is_exhausted(error) {
        Some(FailureKind::QuotaExhausted)
    } else {
        data["status"]
            .as_i64()
            .and_then(|status| i32::try_from(status).ok())
            .and_then(FailureKind::from_http_status)
            .or_else(|| FailureKind::from_message(&detail))
    };
    match kind {
        Some(kind) => AgentError::Provider {
            kind,
            detail,
            resets_at: None,
            retry_after: None,
        },
        None => AgentError::Internal(detail),
    }
}

/// How a failed step or execution ends the prompt.
pub(crate) fn outcome(error: &Value) -> Result<nori_acp_kit::schema::StopReason, AgentError> {
    use nori_acp_kit::schema::StopReason;
    match error["type"].as_str() {
        Some("provider.auth") => Err(AgentError::AuthRequired(
            error["message"]
                .as_str()
                .unwrap_or(
                    "OpenCode's provider rejected the credentials; run `opencode auth login`",
                )
                .to_owned(),
        )),
        Some("aborted") => Ok(StopReason::Cancelled),
        Some("provider.content-filter") => Ok(StopReason::Refusal),
        _ => Err(failure(error)),
    }
}
