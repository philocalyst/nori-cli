//! What the provider behind an agent refuses, and how an agent's settings misbehave.
//!
//! A real agent forwards whatever it is handed to a model API, and the API has limits: a text
//! block of at most so many bytes, a context of at most so many tokens. [`Limits`] gives a fake
//! agent the same limits, answering with the provider's own error body, so a harness that builds
//! an unbounded prompt (a forked history, a compaction checkpoint) fails in a world as it does in
//! production. [`ConfigFault`] is the same idea for `session/set_config_option`: the agent
//! says yes and does something else, or says no in its own words.

use serde::Deserialize;
use serde::Serialize;

use super::agent::ModelDef;
use super::step::ErrorSpec;
use super::value::Span;

/// Whose error bodies a [`Limits`] speaks in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// The OpenAI Responses API (`string_above_max_length`, `context_length_exceeded`).
    #[default]
    Openai,
    /// The Anthropic Messages API (`request_too_large`, `prompt is too long`).
    Anthropic,
}

/// A provider's request limits. A prompt over a limit is refused before any scene runs, with a
/// JSON-RPC internal error whose `data.detail` is the provider's body, as Codex and Claude pass
/// it on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub provider: Provider,
    /// The longest a single text block of the prompt may be, in bytes.
    pub max_text_block_bytes: Option<i64>,
    /// The most all the text blocks of one prompt may add up to, in bytes (about four to a token).
    pub max_prompt_bytes: Option<i64>,
    /// Said in front of the provider's body, as Codex words a failed compaction
    /// (`Error running remote compact task: `).
    pub prefix: Option<String>,
}

impl Limits {
    pub fn is_empty(&self) -> bool {
        self.max_text_block_bytes.is_none() && self.max_prompt_bytes.is_none()
    }
}

/// What an agent does with a `session/set_config_option` it was asked.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigEffect {
    /// Fail the request with `error`.
    #[default]
    Reject,
    /// Say yes and leave the option as it was: the agent keeps running the old model.
    Ignore,
    /// Say yes and set the option to `actual` instead of what was asked.
    Substitute,
}

/// A misbehaving setting. The first fault whose `option` (and `value`, when given) matches the
/// request applies.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigFault {
    /// The option id: `model`, `thought_level`, `mode`, ...
    pub option: String,
    /// Only when this value is asked for; every value when absent.
    pub value: Option<String>,
    pub effect: ConfigEffect,
    /// For `reject`.
    pub error: Option<ErrorSpec>,
    /// For `substitute`: the value the option takes.
    pub actual: Option<String>,
    /// How long the agent takes to answer.
    pub delay: Span,
}

impl Default for ConfigFault {
    fn default() -> Self {
        Self {
            option: String::new(),
            value: None,
            effect: ConfigEffect::Reject,
            error: None,
            actual: None,
            delay: Span::ZERO,
        }
    }
}

/// The agent changes its own settings and says so (`config_option_update`), as Claude does when
/// its CLI finishes starting and reports the models it can run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConfigUpdateStep {
    /// The model list from now on; unchanged when empty. The session keeps its model when it is
    /// still offered, and otherwise takes the first.
    pub models: Vec<ModelDef>,
    /// Options the agent switches by itself, by option id (`model`, `thought_level`, `mode`,
    /// `service_tier`): Claude falling back to another model when one is overloaded, a plan
    /// mode that ends. Each value must be one the option offers.
    pub set: std::collections::BTreeMap<String, String>,
}
