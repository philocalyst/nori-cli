//! A provider's failure as an agent receives it: usually a sentence wrapped around the provider's
//! own JSON body, such as Claude Code's `API Error: 529 {"type":"error","error":{"type":
//! "overloaded_error","message":"Overloaded"}}` or Codex's `unexpected status 400 Bad Request:
//! {"error":{"code":"string_above_max_length","message":"..."}}`.
//!
//! [`ProviderText::parse`] reads the structure out of that text once: the HTTP status, the
//! provider's identifiers (`error.type`, `error.code`), and the sentence a person reads. An agent
//! that reports [`crate::AgentError::provider`] therefore never hands the host a JSON body as the
//! reason, and the class comes from the provider's identifiers before its prose.

use serde_json::Value;

use crate::error::FailureKind;

/// What a provider's error text says, read once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderText {
    /// The HTTP status the text or its body names, if any.
    pub(crate) status: Option<i32>,
    /// The provider's own identifiers for the failure (`overloaded_error`,
    /// `string_above_max_length`), lowercased, most specific first.
    pub(crate) identifiers: Vec<String>,
    /// What a person reads: the provider's sentence when its body has one, else the text itself.
    pub(crate) sentence: String,
}

/// How the provider's identifiers classify the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Identified {
    /// The credential is missing or was refused.
    Auth,
    Kind(FailureKind),
}

impl ProviderText {
    pub(crate) fn parse(text: &str) -> Self {
        let Some((start, body)) = first_object(text) else {
            return Self {
                status: status_in(text),
                identifiers: Vec::new(),
                sentence: text.trim().to_owned(),
            };
        };
        let scopes = [body.get("error"), Some(&body)];
        let mut identifiers = Vec::new();
        for scope in scopes.into_iter().flatten() {
            for field in ["type", "code"] {
                if let Some(identifier) = scope.get(field).and_then(Value::as_str)
                    // Anthropic's envelope is `{"type":"error","error":{...}}`.
                    && identifier != "error"
                {
                    identifiers.push(identifier.to_ascii_lowercase());
                }
            }
        }
        let sentence = scopes
            .into_iter()
            .flatten()
            .find_map(|scope| match scope {
                Value::String(sentence) => Some(sentence.clone()),
                _ => ["message", "detail"]
                    .into_iter()
                    .find_map(|key| scope.get(key)?.as_str().map(str::to_owned)),
            })
            .filter(|sentence| !sentence.trim().is_empty())
            .unwrap_or_else(|| text[..start].trim().trim_end_matches(':').trim().to_owned());
        let status = ["status", "statusCode", "status_code"]
            .into_iter()
            .find_map(|key| {
                body.get(key)
                    .or_else(|| body.get("error")?.get(key))
                    .and_then(Value::as_i64)
            })
            .and_then(|status| i32::try_from(status).ok())
            .or_else(|| status_in(&text[..start]));
        Self {
            status,
            identifiers,
            sentence,
        }
    }

    /// What the provider's identifiers, then its status, say the failure is.
    pub(crate) fn identified(&self) -> Option<Identified> {
        self.identifiers
            .iter()
            .find_map(|identifier| identify(identifier))
            .or_else(|| match self.status {
                Some(401) => Some(Identified::Auth),
                Some(status) => FailureKind::from_http_status(status).map(Identified::Kind),
                None => None,
            })
            .or_else(|| signed_out(&self.sentence).then_some(Identified::Auth))
    }
}

fn identify(identifier: &str) -> Option<Identified> {
    let kind = match identifier {
        "authentication_error" | "invalid_api_key" | "unauthorized" => {
            return Some(Identified::Auth);
        }
        "insufficient_quota"
        | "usage_limit_exceeded"
        | "usage_limit_reached"
        | "billing_hard_limit_reached"
        | "subscription_exhausted" => FailureKind::QuotaExhausted,
        "rate_limit_error" | "rate_limit_exceeded" | "rate_limited" | "too_many_requests" => {
            FailureKind::RateLimited
        }
        "overloaded_error" | "overloaded" | "server_overloaded" | "model_overloaded" => {
            FailureKind::Capacity
        }
        "not_found_error" | "model_not_found" => FailureKind::ModelUnavailable,
        "request_too_large"
        | "string_above_max_length"
        | "context_length_exceeded"
        | "prompt_too_long" => FailureKind::PromptTooLong,
        "api_error" | "server_error" | "internal_server_error" => FailureKind::ServerError,
        _ => return None,
    };
    Some(Identified::Kind(kind))
}

/// Whether `sentence` is a CLI or provider saying the login is missing or no longer good, as
/// Claude Code does in prose (`Not logged in · Please run /login`).
fn signed_out(sentence: &str) -> bool {
    let sentence = sentence.to_lowercase();
    [
        "not logged in",
        "please run /login",
        "invalid api key",
        "oauth token has expired",
        "oauth token has been revoked",
    ]
    .iter()
    .any(|phrase| sentence.contains(phrase))
}

/// The first JSON object in `text` and where it starts.
fn first_object(text: &str) -> Option<(usize, Value)> {
    text.match_indices('{').find_map(|(start, _)| {
        let value = serde_json::Deserializer::from_str(&text[start..])
            .into_iter::<Value>()
            .next()?
            .ok()?;
        value.is_object().then_some((start, value))
    })
}

/// An HTTP error status named in prose: `API Error: 529`, `unexpected status 400 Bad Request`.
fn status_in(text: &str) -> Option<i32> {
    text.split(|c: char| !c.is_ascii_digit())
        .filter(|token| token.len() == 3)
        .filter_map(|token| token.parse::<i32>().ok())
        .find(|status| (400..600).contains(status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn claude_codes_api_error_is_read_as_the_providers_sentence() {
        let text = r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"},"request_id":"req_1"}"#;
        let parsed = ProviderText::parse(text);
        assert_eq!(
            parsed,
            ProviderText {
                status: Some(529),
                identifiers: vec!["overloaded_error".to_owned()],
                sentence: "Overloaded".to_owned(),
            }
        );
        assert_eq!(
            parsed.identified(),
            Some(Identified::Kind(FailureKind::Capacity))
        );
    }

    #[test]
    fn an_openai_body_after_codexs_prefix_is_read_by_its_code() {
        let text = r#"unexpected status 400 Bad Request: {"error":{"message":"Invalid 'input[6].content[0].text': string too long.","type":"invalid_request_error","param":"input[6].content[0].text","code":"string_above_max_length"}}"#;
        let parsed = ProviderText::parse(text);
        assert_eq!(parsed.status, Some(400));
        assert_eq!(
            parsed.sentence,
            "Invalid 'input[6].content[0].text': string too long."
        );
        assert_eq!(
            parsed.identified(),
            Some(Identified::Kind(FailureKind::PromptTooLong))
        );
    }

    #[test]
    fn text_without_a_body_is_its_own_sentence() {
        let parsed =
            ProviderText::parse("exceeded retry limit, last status: 429 Too Many Requests");
        assert_eq!(parsed.status, Some(429));
        assert_eq!(
            parsed.sentence,
            "exceeded retry limit, last status: 429 Too Many Requests"
        );
        assert_eq!(
            parsed.identified(),
            Some(Identified::Kind(FailureKind::RateLimited))
        );
    }

    #[test]
    fn claude_codes_sign_in_prompt_is_an_authentication_failure() {
        assert_eq!(
            ProviderText::parse("Not logged in · Please run /login").identified(),
            Some(Identified::Auth)
        );
    }

    #[test]
    fn a_refused_key_is_an_authentication_failure() {
        let text = r#"API Error: 401 {"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        assert_eq!(
            ProviderText::parse(text).identified(),
            Some(Identified::Auth)
        );
    }
}
