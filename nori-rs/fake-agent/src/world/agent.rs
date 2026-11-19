//! A fake agent: who it claims to be, what it offers, and its script.

use std::sync::OnceLock;

use regex::Regex;
use serde::Deserialize;
use serde::Serialize;

use super::step::ErrorSpec;
use super::step::Step;
use super::value::Span;

/// One model a fake agent offers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelDef {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// The context window reported with usage when a script does not say.
    #[serde(default)]
    pub context_window: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModeDef {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub hint: Option<String>,
}

/// What the agent declares at `initialize`. The kit refuses requests for anything not declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CapabilityDef {
    pub image: bool,
    pub embedded_context: bool,
    pub load: bool,
    pub resume: bool,
    pub fork: bool,
    pub list: bool,
    pub close: bool,
    pub delete_sessions: bool,
    pub additional_directories: bool,
    pub mcp_http: bool,
    /// Whether the agent takes Nori's host tools (only when Nori itself runs it).
    pub host_tools: bool,
}

impl Default for CapabilityDef {
    fn default() -> Self {
        Self {
            image: false,
            embedded_context: false,
            load: true,
            resume: true,
            fork: false,
            list: true,
            close: true,
            delete_sessions: false,
            additional_directories: false,
            mcp_http: false,
            host_tools: true,
        }
    }
}

/// What a prompt that arrives while a turn runs does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPolicyDef {
    /// Refuse it.
    #[default]
    Exclusive,
    /// Join the running turn.
    Steerable,
    /// Cancel the running turn and take its place.
    Replace,
}

/// How the agent starts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Startup {
    /// The `initialize` response is held this long.
    pub initialize_delay: Span,
    /// `session/new` takes this long.
    pub new_session_delay: Span,
    /// Fail `initialize` (the response is an error).
    pub initialize_error: Option<ErrorSpec>,
    /// Fail every `session/new`.
    pub new_session_error: Option<ErrorSpec>,
    /// Require authentication: `session/new` fails with auth required until `authenticate`.
    pub requires_auth: bool,
}

impl Default for Startup {
    fn default() -> Self {
        Self {
            initialize_delay: Span::ZERO,
            new_session_delay: Span::ZERO,
            initialize_error: None,
            new_session_error: None,
            requires_auth: false,
        }
    }
}

/// How a prompt no scene matches is answered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unscripted {
    /// Say what was asked and that no scene matched; recorded as a divergence.
    #[default]
    Echo,
    /// Fail the prompt with an internal error naming the missing scene.
    Fail,
    Hang,
}

/// Which prompts a scene answers.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Match {
    /// Every prompt.
    #[default]
    Always,
    /// The `index`th prompt (0-based) of the session, or of every session of this agent.
    Nth {
        index: i64,
        #[serde(default)]
        per_agent: bool,
    },
    /// Prompts whose text matches the regular expression.
    Matching {
        pattern: String,
        #[serde(skip)]
        compiled: OnceLock<Option<Regex>>,
    },
}

impl PartialEq for Match {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Always, Self::Always) => true,
            (
                Self::Nth { index, per_agent },
                Self::Nth {
                    index: other_index,
                    per_agent: other_per_agent,
                },
            ) => index == other_index && per_agent == other_per_agent,
            (
                Self::Matching { pattern, .. },
                Self::Matching {
                    pattern: other_pattern,
                    ..
                },
            ) => pattern == other_pattern,
            _ => false,
        }
    }
}

impl Clone for Match {
    fn clone(&self) -> Self {
        match self {
            Self::Always => Self::Always,
            Self::Nth { index, per_agent } => Self::Nth {
                index: *index,
                per_agent: *per_agent,
            },
            Self::Matching { pattern, .. } => Self::Matching {
                pattern: pattern.clone(),
                compiled: OnceLock::new(),
            },
        }
    }
}

impl Match {
    /// Whether this scene answers the prompt that is `session_index`th of its session and
    /// `agent_index`th of its agent, with `text`.
    pub fn matches(&self, session_index: i64, agent_index: i64, text: &str) -> bool {
        match self {
            Self::Always => true,
            Self::Nth { index, per_agent } => {
                *index
                    == if *per_agent {
                        agent_index
                    } else {
                        session_index
                    }
            }
            Self::Matching { pattern, compiled } => compiled
                .get_or_init(|| Regex::new(pattern).ok())
                .as_ref()
                .is_some_and(|regex| regex.is_match(text)),
        }
    }

    pub fn pattern_error(&self) -> Option<String> {
        match self {
            Self::Matching { pattern, .. } => Regex::new(pattern)
                .err()
                .map(|error| format!("pattern {pattern:?} is not a regular expression: {error}")),
            Self::Always | Self::Nth { .. } => None,
        }
    }
}

/// What the agent does when the user cancels.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    /// Stop at the next step and answer `cancelled`.
    #[default]
    Honor,
    /// Never notice; the script runs to its end.
    Ignore,
    /// Stop and never answer.
    Hang,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CancelPolicy {
    pub mode: CancelMode,
    /// How long the agent takes to notice.
    pub after: Span,
    /// Steps the agent runs once it has noticed.
    pub then: Vec<Step>,
    /// Run `then` after answering, so the agent keeps talking to a client that thinks it stopped.
    pub detach_then: bool,
}

impl Default for CancelPolicy {
    fn default() -> Self {
        Self {
            mode: CancelMode::Honor,
            after: Span::ZERO,
            then: Vec::new(),
            detach_then: false,
        }
    }
}

/// What the agent does for the prompts it matches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Scene {
    pub name: Option<String>,
    /// Which prompts this scene answers.
    pub on: Match,
    /// Only while the session's model is this one.
    pub model: Option<String>,
    pub cancel: CancelPolicy,
    pub steps: Vec<Step>,
}

impl Default for Scene {
    fn default() -> Self {
        Self {
            name: None,
            on: Match::Always,
            model: None,
            cancel: CancelPolicy::default(),
            steps: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentDef {
    /// The agent's address in the world; filled from the key when agents are a mapping.
    pub slug: String,
    pub title: Option<String>,
    pub version: String,
    pub models: Vec<ModelDef>,
    pub default_model: Option<String>,
    pub modes: Vec<ModeDef>,
    pub default_mode: Option<String>,
    /// Reasoning-effort values; the first is the default.
    pub efforts: Vec<String>,
    pub commands: Vec<CommandDef>,
    pub capabilities: CapabilityDef,
    pub turn_policy: TurnPolicyDef,
    /// The account quota belongs to, reported with usage.
    pub account: Option<String>,
    pub startup: Startup,
    pub unscripted: Unscripted,
    /// Scenes in order: the first whose `on` and `model` match answers the prompt.
    pub scenes: Vec<Scene>,
}

impl Default for AgentDef {
    fn default() -> Self {
        Self {
            slug: String::new(),
            title: None,
            version: "0.1.0".to_owned(),
            models: Vec::new(),
            default_model: None,
            modes: Vec::new(),
            default_mode: None,
            efforts: Vec::new(),
            commands: Vec::new(),
            capabilities: CapabilityDef::default(),
            turn_policy: TurnPolicyDef::Exclusive,
            account: None,
            startup: Startup::default(),
            unscripted: Unscripted::Echo,
            scenes: Vec::new(),
        }
    }
}

impl AgentDef {
    pub fn title(&self) -> &str {
        self.title.as_deref().unwrap_or(&self.slug)
    }

    /// The model a new session starts on.
    pub fn initial_model(&self) -> Option<&ModelDef> {
        self.default_model
            .as_deref()
            .and_then(|id| self.models.iter().find(|model| model.id == id))
            .or_else(|| self.models.first())
    }

    pub fn model(&self, id: &str) -> Option<&ModelDef> {
        self.models.iter().find(|model| model.id == id)
    }
}
