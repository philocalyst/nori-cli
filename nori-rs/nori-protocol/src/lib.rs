//! Public types for embedding a Nori ACP session.
//!
//! ACP owns agent/client semantics. Nori adds only the lifecycle and product
//! notifications that ACP does not define.

mod dial_effect;
mod message_flow;
mod model_controls;
mod monitor;
mod route;
mod session_event;
mod subagent;
mod usage;

pub use agent_client_protocol_schema as acp;
pub use dial_effect::*;
pub use message_flow::*;
pub use model_controls::*;
pub use monitor::*;
pub use route::*;
pub use session_event::*;
pub use subagent::*;
pub use usage::*;

/// ACP `_meta` key correlating a prompt with user-message chunks that echo it.
pub const PROMPT_ECHO_ID_META_KEY: &str = "nori.dev/promptEchoId";
/// Internal marker for a user prompt retried after an engine handoff.
pub const HANDOFF_RETRY_META_KEY: &str = "nori.dev/handoffRetry";

/// Prefix used to carry backend warnings through ACP's standard text stream.
/// The TUI renders these as separate warning cells.
pub const AGENT_WARNING_PREFIX: &str = "\u{001f}nori-warning:\n";

/// Identity of one submitted prompt, carried in `_meta` under [`PROMPT_ECHO_ID_META_KEY`] on the
/// prompt and on every user-message chunk that echoes it.
///
/// A client that shows its prompt before the echo arrives recognizes the echo by this id, never by
/// comparing text.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct PromptEchoId(String);

impl PromptEchoId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `_meta` that carries this id.
    pub fn to_meta(&self) -> acp::v1::Meta {
        acp::v1::Meta::from_iter([(
            PROMPT_ECHO_ID_META_KEY.to_string(),
            serde_json::Value::String(self.0.clone()),
        )])
    }

    /// The id that `meta` carries, if any.
    pub fn in_meta(meta: Option<&acp::v1::Meta>) -> Option<&str> {
        meta?.get(PROMPT_ECHO_ID_META_KEY)?.as_str()
    }
}

/// ACP `_meta` key on an agent message chunk replayed from a transcript: how long the turn it
/// belongs to had been running when the chunk was first observed, in milliseconds, not counting
/// the time the turn spent waiting on the user. A client times a live turn itself; a replayed turn
/// carries its timing with it.
pub const TURN_ELAPSED_META_KEY: &str = "nori.dev/turnElapsedMs";

/// `_meta` that says a replayed update's turn had been running for `elapsed`.
pub fn turn_elapsed_meta(elapsed: std::time::Duration) -> acp::v1::Meta {
    let millis = i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX);
    acp::v1::Meta::from_iter([(
        TURN_ELAPSED_META_KEY.to_string(),
        serde_json::Value::from(millis),
    )])
}

/// How long the turn of the update that carries `meta` had been running, if it says.
pub fn turn_elapsed_in_meta(meta: Option<&acp::v1::Meta>) -> Option<std::time::Duration> {
    let millis = meta?.get(TURN_ELAPSED_META_KEY)?.as_i64()?;
    Some(std::time::Duration::from_millis(
        millis.max(0).try_into().unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn turn_elapsed_round_trips_through_meta() {
        let elapsed = std::time::Duration::from_millis(65_432);
        assert_eq!(
            turn_elapsed_in_meta(Some(&turn_elapsed_meta(elapsed))),
            Some(elapsed)
        );
        assert_eq!(turn_elapsed_in_meta(None), None);
        assert_eq!(
            turn_elapsed_in_meta(Some(&PromptEchoId::new("echo").to_meta())),
            None
        );
    }
}
