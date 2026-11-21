//! A scripted error as the ACP error the host classifies.

use nori_acp_kit::AgentError;
use nori_acp_kit::FailureKind;
use nori_acp_kit::acp;
use serde_json::Value;
use serde_json::json;

use crate::clock::Clock;
use crate::world::ErrorClass;
use crate::world::ErrorSpec;

/// The structured error `spec` describes, with the fields `classify_failure` reads: `code`,
/// `status`, `retry_after`, `resetsAt` and the human `detail`.
pub fn acp_error(spec: &ErrorSpec, clock: &Clock) -> acp::Error {
    let resets_at = spec
        .resets_at
        .clone()
        .or_else(|| spec.resets_in.map(|span| clock.rfc3339_in(span.0)));
    let provider = |kind: FailureKind, detail: &str, status: i64| {
        let error = AgentError::Provider {
            kind,
            detail: spec.detail.clone().unwrap_or_else(|| detail.to_owned()),
            resets_at: resets_at.clone(),
            retry_after: spec.retry_after,
        };
        (error, Some(status))
    };
    let (error, default_status) = match spec.failure {
        ErrorClass::Quota => provider(
            FailureKind::QuotaExhausted,
            "You have reached your usage limit. It resets when the current window ends.",
            429,
        ),
        ErrorClass::ModelUnavailable => provider(
            FailureKind::ModelUnavailable,
            "The model was not found (404); it may be retired or unavailable to this account.",
            404,
        ),
        ErrorClass::ModelPlaceholder => provider(
            FailureKind::ModelUnavailable,
            "Model MODEL_PLACEHOLDER was not found: the model id was never filled in (404).",
            404,
        ),
        ErrorClass::Capacity => provider(
            FailureKind::Capacity,
            "The provider is overloaded at the moment.",
            529,
        ),
        ErrorClass::RateLimited => provider(
            FailureKind::RateLimited,
            "Too many requests; try again shortly.",
            429,
        ),
        ErrorClass::ServerError => provider(
            FailureKind::ServerError,
            "The provider failed with an internal server error.",
            500,
        ),
        ErrorClass::PromptTooLong => provider(
            FailureKind::PromptTooLong,
            "The prompt is too long for the model's context window.",
            413,
        ),
        ErrorClass::Network => (
            AgentError::Unreachable(
                spec.detail
                    .clone()
                    .unwrap_or_else(|| "connection reset by the provider".to_owned()),
            ),
            None,
        ),
        ErrorClass::Auth => (
            AgentError::AuthRequired(
                spec.detail
                    .clone()
                    .unwrap_or_else(|| "authentication required: sign in and retry".to_owned()),
            ),
            None,
        ),
        ErrorClass::SessionNotFound => (
            AgentError::SessionNotFound(
                spec.detail.clone().unwrap_or_else(|| "unknown".to_owned()),
            ),
            None,
        ),
        ErrorClass::NotResumable => (
            AgentError::NotResumable(
                spec.detail
                    .clone()
                    .unwrap_or_else(|| "the session is held by another process".to_owned()),
            ),
            None,
        ),
        ErrorClass::ExecutableNotFound => (
            AgentError::ExecutableNotFound(
                spec.detail
                    .clone()
                    .unwrap_or_else(|| "the agent's program is not installed".to_owned()),
            ),
            None,
        ),
        ErrorClass::NotRunning => (
            AgentError::NotRunning(
                spec.detail
                    .clone()
                    .unwrap_or_else(|| "the service behind the agent is not running".to_owned()),
            ),
            None,
        ),
        ErrorClass::Internal => (
            AgentError::Internal(
                spec.detail
                    .clone()
                    .unwrap_or_else(|| "the agent failed for no reason it can name".to_owned()),
            ),
            None,
        ),
        ErrorClass::Raw => {
            let mut error = acp::Error::new(
                i32::try_from(spec.code.unwrap_or(-32603)).unwrap_or(-32603),
                spec.message.clone().unwrap_or_else(|| "Error".to_owned()),
            );
            error.data = spec.data.clone();
            return error;
        }
    };
    let mut error: acp::Error = error.into();
    if let Some(status) = spec.status.or(default_status)
        && let Some(Value::Object(data)) = &mut error.data
    {
        data.insert("status".to_owned(), json!(status));
    }
    if let Some(Value::Object(data)) = &mut error.data
        && let Some(Value::Object(extra)) = &spec.data
    {
        data.extend(extra.clone());
    }
    error
}
