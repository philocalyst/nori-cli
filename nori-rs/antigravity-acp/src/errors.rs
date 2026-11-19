//! SDK and harness failures as errors the host can act on, without ever touching a credential.

use antigravity_sdk_rust::error::AntigravityError;
use nori_acp_kit::AgentError;
use nori_acp_kit::FailureKind;

/// Nothing in the environment can authenticate the SDK.
pub(crate) fn missing_credentials() -> AgentError {
    AgentError::AuthRequired(
        "Set GEMINI_API_KEY, or GOOGLE_GENAI_USE_VERTEXAI=true with Vertex credentials, or use \
         the default `antigravity` agent (the signed-in official `agy` CLI)."
            .to_owned(),
    )
}

/// Whether the turn ended because the caller cancelled it.
pub(crate) fn is_cancelled(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<AntigravityError>(),
        Some(AntigravityError::Cancelled(_))
    )
}

/// Classify a failure from the SDK or the harness by what it says.
pub(crate) fn classify(error: &anyhow::Error, model: &str) -> AgentError {
    let text = error.to_string();
    let lower = text.to_lowercase();
    if [
        "api key",
        "unauthenticated",
        "permission_denied",
        "401",
        "403",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        return AgentError::AuthRequired(format!(
            "Antigravity rejected the credentials ({text}). Check GEMINI_API_KEY (or the Vertex \
             project and location), or use the default `antigravity` agent (the signed-in \
             official `agy` CLI)."
        ));
    }
    match FailureKind::from_message(&text) {
        Some(FailureKind::ModelUnavailable) => AgentError::Provider {
            kind: FailureKind::ModelUnavailable,
            detail: format!(
                "Antigravity model `{model}` was not found (404): {text}. Pick another model \
                 from the session's model option."
            ),
            resets_at: None,
            retry_after: None,
        },
        Some(kind @ (FailureKind::QuotaExhausted | FailureKind::RateLimited)) => {
            AgentError::Provider {
                kind,
                detail: format!("{text} (model `{model}`)"),
                resets_at: None,
                retry_after: None,
            }
        }
        Some(kind) => AgentError::Provider {
            kind,
            detail: text,
            resets_at: None,
            retry_after: None,
        },
        None => AgentError::Internal(text),
    }
}
