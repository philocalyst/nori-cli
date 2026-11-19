//! Codex failures as the host reads them.
//!
//! A failed turn reaches the host as an ACP error. The host decides whether to retry, fall back to
//! another model or tell the user to sign in from the error's code and data (see
//! `nori_acp_kit::AgentError`), so the failure is classified here from what Codex says it is
//! rather than from its prose. The original `message` (where Codex puts the date a usage limit
//! resets) and `codex_error_info` stay in the data for the host's own checks.

use codex_protocol::protocol::CodexErrorInfo;
use nori_acp_kit::AgentError;
use nori_acp_kit::FailureKind;
use nori_acp_kit::acp::Error;
use serde_json::Value;
use serde_json::json;

/// The error for a turn that failed with `message` and, when Codex classified it, `info`.
pub(super) fn turn_error(message: String, info: Option<&CodexErrorInfo>) -> Error {
    let error: Error = classify(&message, info).into();
    let Some(Value::Object(mut data)) = error.data.clone() else {
        return error;
    };
    data.insert("message".to_owned(), json!(message));
    if let Some(info) = info {
        data.insert("codex_error_info".to_owned(), json!(info));
    }
    error.data(Value::Object(data))
}

fn classify(message: &str, info: Option<&CodexErrorInfo>) -> AgentError {
    let Some(info) = info else {
        return AgentError::provider(None, message);
    };
    let provider = |kind| AgentError::Provider {
        kind,
        detail: message.to_owned(),
        resets_at: None,
        retry_after: None,
    };
    let by_status = |status: &Option<u16>| match status {
        Some(status) => AgentError::provider(Some(i32::from(*status)), message),
        None => provider(FailureKind::Network),
    };
    match info {
        CodexErrorInfo::Unauthorized => AgentError::AuthRequired(message.to_owned()),
        CodexErrorInfo::UsageLimitExceeded | CodexErrorInfo::SessionBudgetExceeded => {
            provider(FailureKind::QuotaExhausted)
        }
        CodexErrorInfo::RateLimitExceeded => provider(FailureKind::RateLimited),
        CodexErrorInfo::ServerOverloaded
        | CodexErrorInfo::FlexUnavailable
        | CodexErrorInfo::PolicyCapacityUnavailable => provider(FailureKind::Capacity),
        CodexErrorInfo::ModelNotFound => provider(FailureKind::ModelUnavailable),
        CodexErrorInfo::ContextWindowExceeded => provider(FailureKind::PromptTooLong),
        CodexErrorInfo::InternalServerError => provider(FailureKind::ServerError),
        CodexErrorInfo::HttpConnectionFailed { http_status_code }
        | CodexErrorInfo::ResponseStreamConnectionFailed { http_status_code }
        | CodexErrorInfo::ResponseStreamDisconnected { http_status_code }
        | CodexErrorInfo::ResponseTooManyFailedAttempts { http_status_code } => {
            by_status(http_status_code)
        }
        CodexErrorInfo::InferenceHookDenied
        | CodexErrorInfo::CyberPolicy
        | CodexErrorInfo::BioPolicy
        | CodexErrorInfo::MisalignmentPolicyViolation
        | CodexErrorInfo::TooManyDenials
        | CodexErrorInfo::BadRequest
        | CodexErrorInfo::InvalidPrompt
        | CodexErrorInfo::SandboxError
        | CodexErrorInfo::ActiveTurnNotSteerable { .. }
        | CodexErrorInfo::ThreadRollbackFailed
        | CodexErrorInfo::Other => AgentError::Internal(message.to_owned()),
    }
}
