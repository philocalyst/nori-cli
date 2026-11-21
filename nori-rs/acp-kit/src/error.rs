//! Errors an agent reports to its host, in the shape the host categorizes.
//!
//! The host classifies a failure from the structured error first and from prose only when nothing
//! structured identifies it (`nori-acp-host`'s `classify_failure`): the JSON-RPC `code`
//! (`-32000` authentication, `-32002` an unknown session, `-32010` an unreachable backing service,
//! `-32012` a session that cannot be reattached, `-32016` a backing service that is not running)
//! and then identifiers in `data`: `code`,
//! `status`, `retry_after`, `resetsAt`. `data.detail` is the reason a person reads. This module
//! writes exactly those fields, so the host's retry and fallback policy never parses prose an
//! agent wrote.

use agent_client_protocol as acp;
use serde_json::Value;
use serde_json::json;

use crate::provider_text::Identified;
use crate::provider_text::ProviderText;

/// `-32010`: the service behind the agent could not be reached.
const UNREACHABLE_CODE: i32 = -32010;
/// `-32012`: the session exists but cannot be reattached.
const NOT_RESUMABLE_CODE: i32 = -32012;
/// `-32016`: the service behind the agent is not running, so there is nothing to reach. The host
/// classifies it like a missing executable: the agent is not available, and waiting does not help.
const NOT_RUNNING_CODE: i32 = -32016;

/// Why a provider call failed, as far as the retry and fallback policy cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Too many requests for now; retrying after a pause works.
    RateLimited,
    /// A usage window or budget is spent; retrying does not work until it resets.
    QuotaExhausted,
    /// The model does not exist, is retired, or this account may not use it.
    ModelUnavailable,
    /// The provider has no capacity right now (overloaded).
    Capacity,
    /// The provider failed on its side (5xx).
    ServerError,
    /// The connection to the provider failed.
    Network,
    /// The prompt does not fit the model's context window.
    PromptTooLong,
}

impl FailureKind {
    /// The identifier the host reads from `data.code`.
    pub fn identifier(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::QuotaExhausted => "usage_limit_reached",
            Self::ModelUnavailable => "model_not_available",
            Self::Capacity => "overloaded",
            Self::ServerError => "server_error",
            Self::Network => "connection_error",
            Self::PromptTooLong => "prompt_too_long",
        }
    }

    /// The class an HTTP status implies. `None` for statuses that say nothing about the
    /// provider's health (a 400 is the caller's own mistake).
    pub fn from_http_status(status: i32) -> Option<Self> {
        match status {
            404 => Some(Self::ModelUnavailable),
            413 => Some(Self::PromptTooLong),
            429 => Some(Self::RateLimited),
            503 | 529 => Some(Self::Capacity),
            500..=599 => Some(Self::ServerError),
            _ => None,
        }
    }

    /// The class a provider's own error message implies, for providers that only send prose.
    pub fn from_message(message: &str) -> Option<Self> {
        let message = message.to_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|needle| message.contains(needle));
        if has(&[
            "usage limit",
            "credit balance is too low",
            "exceeded your usage",
            "out of extra usage",
            "insufficient_quota",
            "quota exceeded",
            "quota exhausted",
            "resource_exhausted",
            "billing_hard_limit",
        ]) {
            Some(Self::QuotaExhausted)
        } else if has(&["rate limit", "rate_limit", "too many requests", "429"]) {
            Some(Self::RateLimited)
        } else if has(&["overloaded", "at capacity", "capacity", "529"]) {
            Some(Self::Capacity)
        } else if has(&[
            "prompt is too long",
            "context length",
            "context window",
            "string too long",
            "string_above_max_length",
            "largestringparam",
        ]) {
            Some(Self::PromptTooLong)
        } else if has(&[
            "model not found",
            "model_not_found",
            "unknown model",
            "no such model",
            "404",
        ]) {
            Some(Self::ModelUnavailable)
        } else if has(&["connection refused", "connection reset", "timed out", "dns"]) {
            Some(Self::Network)
        } else if has(&["internal server error", "bad gateway", "500", "502", "504"]) {
            Some(Self::ServerError)
        } else {
            None
        }
    }
}

/// Everything an agent can fail with. Converts into the `acp::Error` the host expects.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    /// The agent's account is not signed in or its credential was refused.
    #[error("authentication required: {0}")]
    AuthRequired(String),
    /// The session id names nothing this agent holds.
    #[error("unknown session {0}")]
    SessionNotFound(String),
    /// The session exists in storage but cannot be attached again.
    #[error("session cannot be resumed: {0}")]
    NotResumable(String),
    /// The service behind the agent is not reachable.
    #[error("agent backend unreachable: {0}")]
    Unreachable(String),
    /// The service behind the agent is not running at all (nothing listens where it should), so
    /// the person has to start it. Not a network problem.
    #[error("agent backend not running: {0}")]
    NotRunning(String),
    /// The program the agent runs is not installed, so the person has to install it. Reported as
    /// a missing executable with the agent's own install hint, not as a service to start.
    #[error("executable not found: {0}")]
    ExecutableNotFound(String),
    /// The request cannot be served in the state the agent is in, such as a second prompt on a
    /// session that is still running one.
    #[error("{0}")]
    Busy(String),
    /// The request itself is wrong.
    #[error("{0}")]
    InvalidParams(String),
    /// This agent does not do what the request asks, and says so rather than pretending.
    #[error("{0} is not supported by this agent")]
    Unsupported(&'static str),
    /// The provider failed in a way the retry and fallback policy distinguishes.
    #[error("{detail}")]
    Provider {
        kind: FailureKind,
        detail: String,
        /// When a spent window resets, RFC 3339, if the provider says.
        resets_at: Option<String>,
        /// How long to wait before retrying, in seconds, if the provider says.
        retry_after: Option<i64>,
    },
    /// Anything else: a bug or an unclassified failure, with what is known.
    #[error("{0}")]
    Internal(String),
}

impl AgentError {
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::Internal(error.to_string())
    }

    pub fn invalid(detail: impl Into<String>) -> Self {
        Self::InvalidParams(detail.into())
    }

    /// A provider failure, classified from what `message` says. A provider's JSON body inside the
    /// message (Claude Code's `API Error: 529 {...}`, an OpenAI `{"error":{...}}`) is read for its
    /// identifiers and its sentence: the identifiers classify it before `status` and the prose
    /// do, and the sentence, never the body, is the detail a person reads.
    pub fn provider(status: Option<i32>, message: impl Into<String>) -> Self {
        let text = ProviderText::parse(&message.into());
        let identified = text.identified().or_else(|| match status {
            Some(401) => Some(Identified::Auth),
            Some(status) => FailureKind::from_http_status(status).map(Identified::Kind),
            None => None,
        });
        match identified.or_else(|| FailureKind::from_message(&text.sentence).map(Identified::Kind))
        {
            Some(Identified::Auth) => Self::AuthRequired(text.sentence),
            Some(Identified::Kind(kind)) => Self::Provider {
                kind,
                detail: text.sentence,
                resets_at: None,
                retry_after: None,
            },
            None => Self::Internal(text.sentence),
        }
    }

    /// The session is unknown.
    pub fn unknown_session(id: impl std::fmt::Display) -> Self {
        Self::SessionNotFound(id.to_string())
    }
}

fn detail_data(detail: &str) -> Value {
    json!({ "detail": detail })
}

impl From<AgentError> for acp::Error {
    fn from(error: AgentError) -> Self {
        let text = error.to_string();
        match error {
            AgentError::AuthRequired(detail) => {
                acp::Error::auth_required().data(detail_data(&detail))
            }
            AgentError::SessionNotFound(id) => acp::Error::resource_not_found(None)
                .data(detail_data(&format!("unknown session {id}"))),
            AgentError::NotResumable(detail) => {
                acp::Error::new(NOT_RESUMABLE_CODE, "Session not resumable")
                    .data(detail_data(&detail))
            }
            AgentError::Unreachable(detail) => {
                acp::Error::new(UNREACHABLE_CODE, "Agent backend unreachable")
                    .data(detail_data(&detail))
            }
            AgentError::NotRunning(detail) => {
                let mut data = detail_data(&detail);
                if let Some(object) = data.as_object_mut() {
                    object.insert("code".to_owned(), json!("agent_not_running"));
                }
                acp::Error::new(NOT_RUNNING_CODE, "Agent backend not running").data(data)
            }
            AgentError::ExecutableNotFound(detail) => {
                let mut data = detail_data(&detail);
                if let Some(object) = data.as_object_mut() {
                    object.insert("code".to_owned(), json!("executable_not_found"));
                }
                acp::Error::internal_error().data(data)
            }
            AgentError::Busy(detail) | AgentError::InvalidParams(detail) => {
                acp::Error::invalid_params().data(detail_data(&detail))
            }
            AgentError::Unsupported(_) => acp::Error::method_not_found().data(text),
            AgentError::Provider {
                kind,
                detail,
                resets_at,
                retry_after,
            } => {
                let mut data = detail_data(&detail);
                if let Some(object) = data.as_object_mut() {
                    object.insert("code".to_owned(), json!(kind.identifier()));
                    if let Some(resets_at) = resets_at {
                        object.insert("resetsAt".to_owned(), json!(resets_at));
                    }
                    if let Some(seconds) = retry_after {
                        object.insert("retry_after".to_owned(), json!(seconds));
                    }
                }
                acp::Error::internal_error().data(data)
            }
            AgentError::Internal(detail) => acp::Error::internal_error().data(detail_data(&detail)),
        }
    }
}

impl From<std::io::Error> for AgentError {
    fn from(error: std::io::Error) -> Self {
        match error.kind() {
            std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::TimedOut => Self::Unreachable(error.to_string()),
            std::io::ErrorKind::InvalidInput => Self::InvalidParams(error.to_string()),
            _ => Self::Internal(error.to_string()),
        }
    }
}

/// A failed send to the client: the connection is gone, and there is nothing more to say about it.
impl From<acp::Error> for AgentError {
    fn from(error: acp::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<serde_json::Error> for AgentError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn data(error: AgentError) -> Value {
        acp::Error::from(error).data.unwrap_or_default()
    }

    #[test]
    fn a_spent_credit_balance_reaches_the_host_as_its_sentence_and_a_quota() {
        let error = AgentError::provider(
            None,
            r#"API Error: 400 {"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#,
        );
        assert_eq!(
            data(error),
            json!({
                "detail": "Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits.",
                "code": "usage_limit_reached",
            })
        );
    }

    #[test]
    fn a_prompt_too_long_body_is_never_the_detail() {
        let error = AgentError::provider(
            None,
            r#"API Error: 400 {"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 215000 tokens > 200000 maximum"}}"#,
        );
        assert_eq!(
            data(error),
            json!({
                "detail": "prompt is too long: 215000 tokens > 200000 maximum",
                "code": "prompt_too_long",
            })
        );
    }

    #[test]
    fn unclassified_prose_stays_an_internal_error_with_its_words() {
        assert_eq!(
            data(AgentError::provider(None, "something odd happened")),
            json!({"detail": "something odd happened"})
        );
    }
}
