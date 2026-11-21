//! Provider failures, read into a kind a retry policy acts on and a sentence a person reads.

use std::fmt;
use std::time::Duration;

use serde_json::Value;

/// What went wrong, as far as retrying and falling back care.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// The credential is missing, wrong or expired, or not allowed to do this.
    Auth,
    /// Too many requests for now; waiting helps.
    RateLimited,
    /// A budget, credit balance or usage window is spent; waiting does not help soon.
    QuotaExhausted,
    /// The provider has no capacity right now.
    Overloaded,
    /// The provider failed on its side.
    Server,
    /// The prompt does not fit the model's context window.
    PromptTooLong,
    /// The model does not exist or this account may not use it.
    ModelNotFound,
    /// The provider refused the request as malformed.
    InvalidRequest,
    /// A content filter stopped the request.
    ContentFilter,
    /// The connection failed or broke off.
    Network,
    /// The provider stopped answering.
    Timeout,
    /// The provider sent something that could not be read.
    Decode,
}

/// A failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub kind: ErrorKind,
    /// The HTTP status, when the provider answered with one.
    pub status: Option<i32>,
    /// What happened, in the provider's words when it gave any. Never a raw body.
    pub message: String,
    /// Seconds to wait before retrying, when the provider said.
    pub retry_after: Option<i64>,
    /// The provider's own error code or type, such as `insufficient_quota`.
    pub code: Option<String>,
}

impl fmt::Display for ApiError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// Error codes and types that mean a budget is spent rather than a rate exceeded.
const QUOTA_CODES: [&str; 7] = [
    "insufficient_quota",
    "usage_limit_reached",
    "usage_limit_exceeded",
    "billing_hard_limit_reached",
    "quota_exceeded",
    "subscription_exhausted",
    "billing_error",
];

/// Error codes and types that mean the prompt is too large.
const TOO_LONG_CODES: [&str; 4] = [
    "context_length_exceeded",
    "string_above_max_length",
    "request_too_large",
    "model_max_prompt_tokens_exceeded",
];

impl ApiError {
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            status: None,
            message: message.into(),
            retry_after: None,
            code: None,
        }
    }

    /// Whether the same request may succeed if sent again after a pause.
    pub fn is_retryable(&self) -> bool {
        match self.kind {
            ErrorKind::RateLimited
            | ErrorKind::Overloaded
            | ErrorKind::Server
            | ErrorKind::Network
            | ErrorKind::Timeout => true,
            ErrorKind::Auth
            | ErrorKind::QuotaExhausted
            | ErrorKind::PromptTooLong
            | ErrorKind::ModelNotFound
            | ErrorKind::InvalidRequest
            | ErrorKind::ContentFilter
            | ErrorKind::Decode => false,
        }
    }

    pub(crate) fn decode(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Decode, message)
    }

    pub(crate) fn timeout(after: Duration, waiting_for: &str) -> Self {
        Self::new(
            ErrorKind::Timeout,
            format!(
                "The provider sent nothing for {}s while Nori waited for {waiting_for}",
                after.as_secs()
            ),
        )
    }

    pub(crate) fn from_transport(error: reqwest::Error) -> Self {
        // reqwest's own text names the URL, which carries no secret; the cause chain says why.
        let mut message = String::from("Could not reach the provider");
        let mut source: Option<&dyn std::error::Error> = std::error::Error::source(&error);
        let mut reason = None;
        while let Some(cause) = source {
            reason = Some(cause.to_string());
            source = cause.source();
        }
        if let Some(reason) = reason {
            message = format!("{message}: {reason}");
        }
        let kind = if error.is_timeout() {
            ErrorKind::Timeout
        } else {
            ErrorKind::Network
        };
        Self::new(kind, message)
    }

    /// The stream broke off before the provider said it was done.
    pub(crate) fn disconnected(detail: impl fmt::Display) -> Self {
        Self::new(
            ErrorKind::Network,
            format!("The provider's stream broke off before the answer was complete: {detail}"),
        )
    }

    /// An HTTP error response: `status` and its body.
    pub(crate) fn from_http(status: i32, body: &str, retry_after: Option<i64>) -> Self {
        let parsed = serde_json::from_str::<Value>(body).ok();
        let mut error = match &parsed {
            Some(value) => Self::from_body(value, Some(status)),
            None => {
                let text = body.trim();
                let message = if text.is_empty() || text.starts_with('<') || text.len() > 400 {
                    format!("The provider answered HTTP {status}")
                } else {
                    text.to_owned()
                };
                Self::classified(Some(status), None, message)
            }
        };
        error.retry_after = retry_after.or(error.retry_after);
        error
    }

    /// An error the provider reported inside a stream, or as the body of an error response.
    pub(crate) fn from_body(value: &Value, status: Option<i32>) -> Self {
        let error = value
            .get("error")
            .filter(|error| error.is_object())
            .unwrap_or(value);
        let message = [
            error.get("message"),
            value.get("message"),
            value.get("detail"),
        ]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(ToOwned::to_owned);
        let code = [error.get("code"), error.get("type"), value.get("code")]
            .into_iter()
            .flatten()
            .find_map(|code| match code {
                Value::String(code) if code != "error" => Some(code.clone()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            });
        let status = status.or_else(|| {
            [error.get("status"), error.get("code"), value.get("status")]
                .into_iter()
                .flatten()
                .find_map(Value::as_i64)
                .and_then(|status| i32::try_from(status).ok())
                .filter(|status| (400..600).contains(status))
        });
        let message = message.unwrap_or_else(|| match status {
            Some(status) => format!("The provider answered HTTP {status}"),
            None => "The provider reported an error without saying what it was".to_owned(),
        });
        Self::classified(status, code, message)
    }

    fn classified(status: Option<i32>, code: Option<String>, message: String) -> Self {
        let lowered = message.to_lowercase();
        let has = |needles: &[&str]| needles.iter().any(|needle| lowered.contains(needle));
        let code_is = |codes: &[&str]| {
            code.as_deref()
                .is_some_and(|code| codes.iter().any(|known| code.eq_ignore_ascii_case(known)))
        };
        let kind = if code_is(&QUOTA_CODES)
            || has(&[
                "credit balance is too low",
                "exceeded your current quota",
                "insufficient credits",
                "insufficient balance",
                "usage limit",
            ]) {
            ErrorKind::QuotaExhausted
        } else if code_is(&["overloaded_error", "overloaded"]) || matches!(status, Some(503 | 529))
        {
            ErrorKind::Overloaded
        } else if code_is(&TOO_LONG_CODES)
            || status == Some(413)
            || has(&[
                "prompt is too long",
                "maximum context length",
                "context window",
                "context length",
                "too many tokens",
                "string too long",
            ])
        {
            ErrorKind::PromptTooLong
        } else if matches!(status, Some(401 | 403))
            || code_is(&[
                "authentication_error",
                "permission_error",
                "invalid_api_key",
                "unauthorized",
            ])
        {
            ErrorKind::Auth
        } else if status == Some(429) || code_is(&["rate_limit_error", "rate_limit_exceeded"]) {
            ErrorKind::RateLimited
        } else if status == Some(404) || code_is(&["model_not_found", "not_found_error"]) {
            ErrorKind::ModelNotFound
        } else if code_is(&["content_filter", "content_policy_violation"]) {
            ErrorKind::ContentFilter
        } else if code_is(&["api_error", "server_error", "internal_server_error"])
            || status.is_some_and(|status| status >= 500)
        {
            ErrorKind::Server
        } else {
            ErrorKind::InvalidRequest
        };
        Self {
            kind,
            status,
            message,
            retry_after: None,
            code,
        }
    }
}

/// The wait a response's headers ask for, in seconds.
pub(crate) fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok());
    if let Some(milliseconds) = header("retry-after-ms").and_then(|value| value.parse::<f64>().ok())
    {
        return Some((milliseconds / 1000.0).ceil() as i64);
    }
    header("retry-after")
        .and_then(|value| value.trim().parse::<f64>().ok())
        .map(|seconds| seconds.ceil() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn anthropic_credit_balance_is_a_spent_quota_in_prose() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}"#;
        let error = ApiError::from_http(400, body, None);
        assert_eq!(error.kind, ErrorKind::QuotaExhausted);
        assert!(error.message.starts_with("Your credit balance is too low"));
    }

    #[test]
    fn overloaded_stream_event_is_capacity() {
        let event: Value = serde_json::from_str(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        )
        .unwrap_or_default();
        let error = ApiError::from_body(&event, None);
        assert_eq!(
            error,
            ApiError {
                kind: ErrorKind::Overloaded,
                status: None,
                message: "Overloaded".to_owned(),
                retry_after: None,
                code: Some("overloaded_error".to_owned()),
            }
        );
        assert!(error.is_retryable());
    }

    #[test]
    fn openai_string_too_long_is_prompt_too_long() {
        let body = r#"{"error":{"message":"Invalid 'input[3].content[0].text': string too long. Expected a string with maximum length 10485760, but got a string with length 12000000 instead.","type":"invalid_request_error","param":"input[3].content[0].text","code":"string_above_max_length"}}"#;
        let error = ApiError::from_http(400, body, None);
        assert_eq!(error.kind, ErrorKind::PromptTooLong);
        assert_eq!(error.code.as_deref(), Some("string_above_max_length"));
    }

    #[test]
    fn insufficient_quota_429_is_not_a_rate_limit() {
        let body = r#"{"error":{"message":"You exceeded your current quota.","type":"insufficient_quota","code":"insufficient_quota"}}"#;
        assert_eq!(
            ApiError::from_http(429, body, Some(3)).kind,
            ErrorKind::QuotaExhausted
        );
        let limited = ApiError::from_http(429, r#"{"error":{"message":"slow down"}}"#, Some(3));
        assert_eq!(
            (limited.kind, limited.retry_after),
            (ErrorKind::RateLimited, Some(3))
        );
    }

    #[test]
    fn html_bodies_are_not_shown() {
        let error = ApiError::from_http(502, "<html><body>Bad gateway</body></html>", None);
        assert_eq!(
            (error.kind, error.message.as_str()),
            (ErrorKind::Server, "The provider answered HTTP 502")
        );
    }
}
