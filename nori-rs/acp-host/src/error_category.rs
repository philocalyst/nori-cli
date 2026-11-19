//! Classifying failed requests.
//!
//! Every failure of an ACP request lands in one [`FailureClass`], from the structured error the
//! agent sent (JSON-RPC code, `data` identifiers, HTTP status) first and from message text only
//! when nothing structured identifies it. The recovery policy (`nori-workflow`) decides what to
//! do per [`FailureKind`]; nothing else in Nori re-reads error strings.

use std::time::Duration;

use nori_protocol::acp::v1 as acp;
use nori_workflow::FailureKind;

/// What went wrong with a request, with the facts the agent gave about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FailureClass {
    /// Throttled; `retry_after` is the provider's own hint for how long to wait.
    RateLimited { retry_after: Option<Duration> },
    /// The subscription or billing window is used up; `resets_at` is when the agent says it
    /// renews, verbatim.
    QuotaExhausted { resets_at: Option<String> },
    /// The model is gone on this route: HTTP 404, a stale id, an unfilled `MODEL_PLACEHOLDER`.
    ModelUnavailable,
    /// The provider has no capacity for the model right now (overloaded, HTTP 529).
    Capacity,
    /// The provider failed internally (HTTP 5xx).
    ServerError,
    /// The agent's backing service could not be reached (ACP `-32010`, resets, timeouts).
    Network,
    /// Credentials missing, expired or rejected (ACP `-32000`, HTTP 401).
    Auth,
    /// The prompt exceeds the model's context window.
    PromptTooLong,
    /// The agent executable could not be found.
    ExecutableNotFound,
    /// The agent failed to initialise or speak ACP.
    Initialization,
    /// The requested session no longer exists on the agent (ACP `-32002`).
    SessionNotFound,
    /// The session exists but cannot be reattached (ACP `-32012`).
    SessionNotResumable,
    /// No session is active on the agent connection (ACP `-32014`).
    NoActiveSession,
    /// A session is already active on the agent connection (ACP `-32015`).
    SessionAlreadyActive,
    /// Nothing identified the failure.
    Unknown,
}

impl FailureClass {
    /// The class without its data, the vocabulary of recovery policy.
    pub fn kind(&self) -> FailureKind {
        match self {
            FailureClass::RateLimited { .. } => FailureKind::RateLimited,
            FailureClass::QuotaExhausted { .. } => FailureKind::QuotaExhausted,
            FailureClass::ModelUnavailable => FailureKind::ModelUnavailable,
            FailureClass::Capacity => FailureKind::Capacity,
            FailureClass::ServerError => FailureKind::ServerError,
            FailureClass::Network => FailureKind::Network,
            FailureClass::Auth => FailureKind::Auth,
            FailureClass::PromptTooLong => FailureKind::PromptTooLong,
            FailureClass::ExecutableNotFound => FailureKind::ExecutableNotFound,
            FailureClass::Initialization => FailureKind::Initialization,
            FailureClass::SessionNotFound => FailureKind::SessionNotFound,
            FailureClass::SessionNotResumable => FailureKind::SessionNotResumable,
            FailureClass::NoActiveSession => FailureKind::NoActiveSession,
            FailureClass::SessionAlreadyActive => FailureKind::SessionAlreadyActive,
            FailureClass::Unknown => FailureKind::Unknown,
        }
    }

    /// Whether the condition is transient, so that a caller with no policy (an unattended
    /// loop that must survive a blip) may try again later. Limits ease over time; everything
    /// else reflects a persistent problem that waiting cannot fix.
    pub fn is_transient(&self) -> bool {
        match self {
            FailureClass::RateLimited { .. }
            | FailureClass::QuotaExhausted { .. }
            | FailureClass::Capacity
            | FailureClass::ServerError
            | FailureClass::Network => true,
            FailureClass::ModelUnavailable
            | FailureClass::Auth
            | FailureClass::PromptTooLong
            | FailureClass::ExecutableNotFound
            | FailureClass::Initialization
            | FailureClass::SessionNotFound
            | FailureClass::SessionNotResumable
            | FailureClass::NoActiveSession
            | FailureClass::SessionAlreadyActive
            | FailureClass::Unknown => false,
        }
    }

    /// The provider's hint for how long to wait before retrying, when it gave one.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            FailureClass::RateLimited { retry_after } => *retry_after,
            FailureClass::QuotaExhausted { .. }
            | FailureClass::ModelUnavailable
            | FailureClass::Capacity
            | FailureClass::ServerError
            | FailureClass::Network
            | FailureClass::Auth
            | FailureClass::PromptTooLong
            | FailureClass::ExecutableNotFound
            | FailureClass::Initialization
            | FailureClass::SessionNotFound
            | FailureClass::SessionNotResumable
            | FailureClass::NoActiveSession
            | FailureClass::SessionAlreadyActive
            | FailureClass::Unknown => None,
        }
    }
}

/// A classified failure, carrying the agent-supplied message and `error.data.detail` (for
/// example a login hint) when the chain includes a structured ACP error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureDetails {
    pub class: FailureClass,
    pub message: Option<String>,
    pub detail: Option<String>,
}

/// Whether an ACP error explicitly identifies exhausted provider usage.
///
/// A generic 429 or rate-limit message can mean a short-lived throttle and must not exclude a
/// provider from a conversation. Only structured identifiers count, never arbitrary text.
pub fn is_subscription_exhausted(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<acp::Error>()
            .and_then(classify_structured)
            .is_some_and(|class| matches!(class, FailureClass::QuotaExhausted { .. }))
    })
}

/// Classify an error, preferring the structured ACP error (JSON-RPC `code` + `data`) in the
/// anyhow chain over text heuristics.
///
/// The code table mirrors the agent side (`nori-handroll acp`): `-32000` auth_required,
/// `-32002` resource/session not found, `-32010` backing service unreachable, `-32012`
/// session not resumable, `-32014` no active session, `-32015` a session is already active.
pub fn classify_failure(error: &anyhow::Error) -> FailureDetails {
    for cause in error.chain() {
        let Some(acp_error) = cause.downcast_ref::<acp::Error>() else {
            continue;
        };
        // Agents say why in `data.detail`; Codex's in-process agent puts it in `data.message`
        // under a generic "Internal error", which is where the usage-limit reset date lives, and
        // the Claude driver makes `data` the reason itself.
        let detail = acp_error.data.as_ref().and_then(|data| match data {
            serde_json::Value::String(reason) => Some(reason.clone()),
            _ => ["detail", "message"].into_iter().find_map(|key| {
                data.get(key)
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            }),
        });
        let class = classify_structured(acp_error)
            .unwrap_or_else(|| classify_text(&format!("{acp_error:?}")));
        return FailureDetails {
            class,
            message: Some(acp_error.message.clone()),
            detail,
        };
    }
    FailureDetails {
        class: classify_text(&format!("{error:?}")),
        message: None,
        detail: None,
    }
}

/// What the structured parts of an ACP error say, if they identify the failure.
fn classify_structured(error: &acp::Error) -> Option<FailureClass> {
    // ACP lifecycle and authentication codes take precedence over any provider-shaped field.
    match i32::from(error.code) {
        -32000 => return Some(FailureClass::Auth),
        -32002 => return Some(FailureClass::SessionNotFound),
        -32010 => return Some(FailureClass::Network),
        -32012 => return Some(FailureClass::SessionNotResumable),
        -32014 => return Some(FailureClass::NoActiveSession),
        -32015 => return Some(FailureClass::SessionAlreadyActive),
        _ => {}
    }
    let data = error.data.as_ref();
    let scopes = || {
        data.into_iter()
            .flat_map(|data| [Some(data), data.get("error"), data.get("response")])
            .flatten()
    };
    let identifiers = scopes().flat_map(|scope| {
        let info = scope.get("codex_error_info");
        let from_info = match info {
            Some(serde_json::Value::String(name)) => Some(name.as_str()),
            Some(serde_json::Value::Object(map)) => map.keys().next().map(String::as_str),
            _ => None,
        };
        [from_info]
            .into_iter()
            .chain(
                ["code", "type"]
                    .into_iter()
                    .map(|field| scope.get(field).and_then(serde_json::Value::as_str)),
            )
            .flatten()
    });
    for identifier in identifiers {
        if let Some(class) = classify_identifier(&identifier.to_ascii_lowercase(), data) {
            return Some(class);
        }
    }
    let status = scopes()
        .flat_map(|scope| {
            ["status", "statusCode", "status_code", "http_status"].map(|f| scope.get(f))
        })
        .flatten()
        .find_map(serde_json::Value::as_i64)
        .or_else(|| {
            let code = i64::from(i32::from(error.code));
            (400..600).contains(&code).then_some(code)
        });
    status.and_then(|status| classify_status(status, data))
}

fn classify_identifier(identifier: &str, data: Option<&serde_json::Value>) -> Option<FailureClass> {
    Some(match identifier {
        "insufficient_quota"
        | "usage_limit_exceeded"
        | "usage_limit_reached"
        | "billing_hard_limit_reached"
        | "subscription_exhausted" => FailureClass::QuotaExhausted {
            resets_at: data.and_then(resets_at),
        },
        "rate_limit_error" | "rate_limit_exceeded" | "rate_limited" | "too_many_requests" => {
            FailureClass::RateLimited {
                retry_after: data.and_then(retry_after),
            }
        }
        "overloaded_error" | "overloaded" | "server_overloaded" | "model_overloaded" => {
            FailureClass::Capacity
        }
        "api_error" | "server_error" | "internal_server_error" | "service_unavailable" => {
            FailureClass::ServerError
        }
        "http_connection_failed"
        | "response_stream_connection_failed"
        | "response_stream_disconnected"
        | "connection_error"
        | "timeout_error"
        | "request_timeout" => FailureClass::Network,
        "authentication_error" | "unauthorized" | "invalid_api_key" => FailureClass::Auth,
        "not_found_error" | "model_not_found" | "model_not_available" | "unsupported_model" => {
            FailureClass::ModelUnavailable
        }
        "context_window_exceeded"
        | "context_length_exceeded"
        | "prompt_too_long"
        | "request_too_large" => FailureClass::PromptTooLong,
        _ => return None,
    })
}

fn classify_status(status: i64, data: Option<&serde_json::Value>) -> Option<FailureClass> {
    Some(match status {
        401 | 403 => FailureClass::Auth,
        404 => FailureClass::ModelUnavailable,
        408 => FailureClass::Network,
        413 => FailureClass::PromptTooLong,
        429 => FailureClass::RateLimited {
            retry_after: data.and_then(retry_after),
        },
        500 | 502 | 503 | 504 => FailureClass::ServerError,
        529 => FailureClass::Capacity,
        _ => return None,
    })
}

/// The provider's own wait hint, from `retry_after` (seconds), `retry_after_ms` or a
/// `retry-after` header, at the top of the error data or under `error` / `response`.
fn retry_after(data: &serde_json::Value) -> Option<Duration> {
    [Some(data), data.get("error"), data.get("response")]
        .into_iter()
        .flatten()
        .find_map(|scope| {
            let seconds = ["retry_after", "retryAfter", "retry_after_seconds"]
                .iter()
                .find_map(|field| scope.get(field))
                .or_else(|| scope.get("headers")?.get("retry-after"))
                .and_then(|value| match value {
                    serde_json::Value::Number(number) => number.as_f64(),
                    serde_json::Value::String(text) => text.trim().parse().ok(),
                    _ => None,
                });
            let millis = scope
                .get("retry_after_ms")
                .and_then(serde_json::Value::as_f64);
            seconds
                .map(|seconds| seconds * 1000.0)
                .or(millis)
                .filter(|millis| millis.is_finite() && *millis >= 0.0)
                .map(|millis| Duration::from_millis(millis as u64))
        })
}

/// When the agent says the exhausted usage renews, verbatim (RFC 3339 or epoch seconds).
fn resets_at(data: &serde_json::Value) -> Option<String> {
    [
        Some(data),
        data.get("error"),
        data.get("rate_limit_info"),
        data.get("response"),
    ]
    .into_iter()
    .flatten()
    .find_map(|scope| {
        ["resets_at", "resetsAt", "reset_at"]
            .iter()
            .find_map(|field| scope.get(field))
            .and_then(|value| match value {
                serde_json::Value::String(text) => Some(text.clone()),
                serde_json::Value::Number(number) => Some(number.to_string()),
                _ => None,
            })
    })
}

/// Whether `text` names HTTP status `code` as a standalone number, so `5000 tokens` is not a 500.
fn mentions_status(text: &str, code: &str) -> bool {
    text.match_indices(code).any(|(at, _)| {
        let before = text[..at].chars().next_back();
        let after = text[at + code.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

/// Classify from message text alone: the last resort, for errors that carry no structure.
pub fn classify_text(error: &str) -> FailureClass {
    let text = error.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
    let status = |codes: &[&str]| codes.iter().any(|code| mentions_status(&text, code));

    if has(&[
        "model_placeholder",
        "model not found",
        "model_not_found",
        "unknown model",
        "no such model",
        "is not a valid model",
        "model does not exist",
        "model is not available",
    ]) || (status(&["404"]) && has(&["model", "not found"]))
    {
        FailureClass::ModelUnavailable
    } else if has(&[
        "prompt is too long",
        "context window",
        "context length",
        "context_length",
        "maximum context",
    ]) {
        FailureClass::PromptTooLong
    } else if has(&[
        "usage limit",
        "out of extra usage",
        "exceeded your usage",
        "insufficient_quota",
        "exceeded your current quota",
        "quota exceeded",
        "quota exhausted",
        "quota",
    ]) {
        FailureClass::QuotaExhausted { resets_at: None }
    } else if has(&["rate limit", "rate_limit", "too many requests"]) || status(&["429"]) {
        FailureClass::RateLimited { retry_after: None }
    } else if has(&["overloaded", "at capacity", "no capacity", "capacity"]) || status(&["529"]) {
        FailureClass::Capacity
    } else if has(&[
        "unauthorized",
        "unauthenticated",
        "authentication",
        "authorization",
        "auth_required",
        "auth required",
        "api key",
        "not logged in",
        "-32000",
    ]) || status(&["401", "403"])
    {
        FailureClass::Auth
    } else if has(&[
        "command not found",
        "os error 2",           // ENOENT on Unix
        "cannot find the path", // Windows
    ]) || (has(&["no such file"]) && has(&["directory"]))
    {
        FailureClass::ExecutableNotFound
    } else if has(&[
        "connection refused",
        "connection reset",
        "connection closed",
        "broken pipe",
        "timed out",
        "timeout",
        "unreachable",
        "network",
        "dns error",
        "error sending request",
    ]) || status(&["408"])
    {
        FailureClass::Network
    } else if has(&[
        "server error",
        "api_error",
        "internal server error",
        "service unavailable",
        "bad gateway",
        "gateway timeout",
    ]) || status(&["500", "502", "503", "504"])
    {
        FailureClass::ServerError
    } else if has(&["initialization", "handshake", "protocol error"]) {
        FailureClass::Initialization
    } else {
        FailureClass::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Wrap a structured ACP error the way connection methods surface it: as
    /// the source of an anyhow chain with a human context message on top.
    fn chain_with(acp_error: acp::Error) -> anyhow::Error {
        anyhow::Error::new(acp_error).context("Failed to resume ACP session")
    }

    #[test]
    fn auth_required_categorizes_as_authentication_with_login_detail() {
        let error = chain_with(
            acp::Error::auth_required()
                .data(serde_json::json!({ "detail": "run: nori-handroll login" })),
        );

        let details = classify_failure(&error);

        assert_eq!(
            (
                details.class,
                details.message.as_deref(),
                details.detail.as_deref()
            ),
            (
                FailureClass::Auth,
                Some("Authentication required"),
                Some("run: nori-handroll login")
            )
        );
    }

    #[test]
    fn session_lifecycle_codes_map_to_dedicated_categories() {
        let cases = [
            (-32002, FailureClass::SessionNotFound),
            (-32010, FailureClass::Network),
            (-32012, FailureClass::SessionNotResumable),
            (-32014, FailureClass::NoActiveSession),
            (-32015, FailureClass::SessionAlreadyActive),
        ];
        for (code, expected) in cases {
            let error = chain_with(acp::Error::new(code, "structured error"));
            assert_eq!(classify_failure(&error).class, expected, "code {code}");
        }
    }

    #[test]
    fn structured_code_takes_precedence_over_message_heuristics() {
        // The message alone would substring-match the auth heuristics; the
        // structured code must win.
        let error = chain_with(acp::Error::new(-32002, "unauthorized session"));

        assert_eq!(
            classify_failure(&error).class,
            FailureClass::SessionNotFound
        );
    }

    #[test]
    fn a_reason_given_as_the_whole_data_is_the_detail() {
        let error = anyhow::Error::new(acp::Error::internal_error().data(serde_json::json!(
            "Claude Code CLI not found. Please ensure 'claude' is in your PATH"
        )));
        let details = classify_failure(&error);
        assert_eq!(
            details.detail.as_deref(),
            Some("Claude Code CLI not found. Please ensure 'claude' is in your PATH")
        );
    }

    #[test]
    fn falls_back_to_string_heuristics_without_structured_error() {
        let error = anyhow::anyhow!("429 too many requests");

        let details = classify_failure(&error);

        assert_eq!(
            (details.class, details.message, details.detail),
            (FailureClass::RateLimited { retry_after: None }, None, None)
        );
    }

    #[test]
    fn broker_unreachable_is_retryable_but_session_states_are_not() {
        assert!(FailureClass::Network.is_transient());
        assert!(!FailureClass::SessionNotFound.is_transient());
        assert!(!FailureClass::SessionNotResumable.is_transient());
        assert!(!FailureClass::SessionAlreadyActive.is_transient());
        assert!(!FailureClass::NoActiveSession.is_transient());
    }

    #[test]
    fn subscription_exhaustion_requires_a_structured_provider_identifier() {
        for data in [
            serde_json::json!({"code": "usage_limit_reached"}),
            serde_json::json!({"error": {"type": "insufficient_quota"}}),
            serde_json::json!({"error": {"code": "usage_limit_exceeded"}}),
            serde_json::json!({"codex_error_info": "usage_limit_exceeded"}),
            serde_json::json!({
                "error": {"code": "usage_limit_exceeded"},
                "provider": "anthropic",
                "rate_limit_info": {"status": "rejected", "rateLimitType": "five_hour"}
            }),
        ] {
            assert!(is_subscription_exhausted(&chain_with(
                acp::Error::internal_error().data(data)
            )));
        }
        for error in [
            chain_with(acp::Error::new(429, "too many requests")),
            chain_with(
                acp::Error::internal_error().data(serde_json::json!({"code": "rate_limit_error"})),
            ),
            chain_with(
                acp::Error::internal_error()
                    .data(serde_json::json!({"codex_error_info": "rate_limit_exceeded"})),
            ),
            chain_with(
                acp::Error::auth_required().data(serde_json::json!({"code": "insufficient_quota"})),
            ),
            anyhow::anyhow!("usage_limit_reached"),
        ] {
            assert!(!is_subscription_exhausted(&error));
        }
    }
}
