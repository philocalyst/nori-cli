//! What makes an engine agent Codex or OpenCode: its models, credentials, configuration, prompt
//! and tools. Everything else (the loop, sessions, permissions, the ACP surface) is the engine's.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nori_acp_kit::AgentError;
use nori_acp_kit::AuthStatus;
use nori_acp_kit::RateLimitWindow;
use nori_acp_kit::schema::AuthMethod;
use serde_json::Map;
use serde_json::Value;

use crate::rules::Rule;
use crate::tools::Tool;

/// Who the agent is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// The wire name, such as `nori-opencode`.
    pub name: &'static str,
    /// What a person calls it.
    pub title: &'static str,
    /// The directory under `$NORI_HOME/agents/` that holds its sessions.
    pub store: &'static str,
}

/// A model the agent offers.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    /// The value of the model option, unique within the agent: `provider/model`.
    pub id: String,
    /// What a person reads: `Anthropic · Claude Sonnet 5`.
    pub label: String,
    pub description: Option<String>,
    /// The context window, in tokens, when known.
    pub context_window: Option<i64>,
    /// The most the model writes in one answer, when known.
    pub max_output_tokens: Option<i64>,
    /// The reasoning efforts the model accepts, weakest first; empty when it does not reason
    /// or takes no setting.
    pub efforts: Vec<String>,
    pub default_effort: Option<String>,
    /// Whether it reads images.
    pub images: bool,
}

/// How an effort setting reaches the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortStyle {
    /// As the effort's own name (OpenAI `reasoning.effort`, `reasoning_effort`).
    Named,
    /// As an Anthropic thinking budget derived from the effort.
    AnthropicBudget,
}

/// Everything needed to send one model request, credentials resolved.
#[derive(Debug, Clone)]
pub struct Connection {
    pub endpoint: nori_llm::Endpoint,
    /// The model as the provider names it.
    pub model: String,
    pub effort_style: EffortStyle,
    /// Chat Completions: the field reasoning is echoed back in, for models that want it.
    pub reasoning_echo_field: Option<String>,
    /// Provider and model body settings.
    pub extra_body: Map<String, Value>,
    pub temperature: Option<f64>,
    /// Who pays, for the usage report (`you@example.com`, `OpenRouter key`).
    pub account: Option<String>,
    /// Whether requests state the most the model may write. ChatGPT's Codex backend refuses the
    /// field, so its connections leave it out.
    pub output_limit: bool,
}

/// A setting a flavor offers beyond the model, effort and mode (Codex's service tier): a select
/// whose value goes into every request body as one field.
#[derive(Debug, Clone, PartialEq)]
pub struct Dial {
    /// The option id (`service_tier`).
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// The ACP option category (`model_config`).
    pub category: nori_acp_kit::schema::SessionConfigOptionCategory,
    /// The request body field the selected value goes in.
    pub field: String,
    /// The choices, in the order a picker shows them. A model is offered the ones that name it
    /// or name no model.
    pub choices: Vec<DialChoice>,
    /// The choice a session starts with, and falls back to when a model switch leaves the
    /// current one unoffered.
    pub default: String,
}

/// One choice of a [`Dial`].
#[derive(Debug, Clone, PartialEq)]
pub struct DialChoice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    /// What the request body's field is set to; `None` leaves the field out (the provider's own
    /// default).
    pub body: Option<Value>,
    /// The models (ids) that offer it; empty for every model.
    pub models: Vec<String>,
}

impl Dial {
    /// The choices `model` is offered.
    pub fn choices_for<'a>(&'a self, model: &'a str) -> impl Iterator<Item = &'a DialChoice> + 'a {
        self.choices.iter().filter(move |choice| {
            choice.models.is_empty() || choice.models.iter().any(|id| id == model)
        })
    }
}

/// A mode the agent runs in: a name, the rules it adds and what it tells the model.
#[derive(Debug, Clone, PartialEq)]
pub struct Mode {
    pub id: String,
    pub label: String,
    pub description: Option<String>,
    /// Rules on top of the workspace's.
    pub rules: Vec<Rule>,
    /// Extra system prompt.
    pub prompt: Option<String>,
    /// The model this mode runs with, when it names one.
    pub model: Option<String>,
}

/// Text the model is told about the project, and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub source: PathBuf,
    pub text: String,
}

/// A slash command: a prompt template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandDef {
    pub name: String,
    pub description: String,
    /// The prompt, with `$ARGUMENTS` and `$1`..`$9` for what follows the command.
    pub template: String,
}

impl CommandDef {
    /// The prompt for `arguments`.
    pub fn expand(&self, arguments: &str) -> String {
        let words: Vec<&str> = arguments.split_whitespace().collect();
        let takes_arguments = self.template.contains("$ARGUMENTS")
            || (1..=9).any(|index| self.template.contains(&format!("${index}")));
        let mut text = self.template.replace("$ARGUMENTS", arguments);
        for index in (1..=9).rev() {
            let word = words.get(index - 1).copied().unwrap_or("");
            text = text.replace(&format!("${index}"), word);
        }
        if !takes_arguments && !arguments.trim().is_empty() {
            text.push_str("\n\n");
            text.push_str(arguments);
        }
        text
    }
}

/// What a flavor knows about one working directory.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    /// The models offered, in the order a picker shows them.
    pub models: Vec<ModelInfo>,
    /// The model a new session starts with.
    pub default_model: Option<String>,
    pub modes: Vec<Mode>,
    pub default_mode: Option<String>,
    /// Base permission rules.
    pub rules: Vec<Rule>,
    /// Project and user instructions (`AGENTS.md` and the like).
    pub instructions: Vec<Instruction>,
    pub commands: Vec<CommandDef>,
    /// MCP servers the flavor's own configuration names, started with the client's.
    pub mcp_servers: Vec<nori_acp_kit::schema::McpServer>,
    /// Things a person should know about the configuration (a file that did not parse).
    pub warnings: Vec<String>,
    /// Settings beyond the model, effort and mode, each sent as a request body field.
    pub dials: Vec<Dial>,
}

impl Workspace {
    pub fn model(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|model| model.id == id)
    }

    pub fn dial(&self, id: &str) -> Option<&Dial> {
        self.dials.iter().find(|dial| dial.id == id)
    }
}

/// The agent-specific half of an engine agent.
#[async_trait]
pub trait Flavor: Send + Sync + 'static {
    fn identity(&self) -> Identity;

    /// Models, modes, rules, instructions and commands for `cwd`. Called when a session opens
    /// and may read files: implementations do blocking work off the async runtime.
    async fn workspace(&self, cwd: &Path) -> Result<Workspace, AgentError>;

    /// Ticks when what [`Flavor::workspace`] returns changes on its own (a newer model
    /// catalog), so every open session is offered it at once. `None` for a workspace that is
    /// read only when a session opens.
    fn workspace_changes(&self) -> Option<tokio::sync::watch::Receiver<i64>> {
        None
    }

    /// The endpoint for `model` (an id from `cwd`'s workspace), with its credential resolved
    /// now: a token that expired is refreshed here.
    async fn connect(&self, cwd: &Path, model: &str) -> Result<Connection, AgentError>;

    /// Who pays for the agent's turns.
    async fn auth_status(&self) -> AuthStatus;

    /// The base system prompt for `model`.
    fn system_prompt(&self, model: &ModelInfo) -> String;

    /// The built-in tools the model gets (host tools are added by the engine).
    fn tools(&self) -> Vec<Arc<dyn Tool>>;

    /// How the shared file tools behave for this agent (an agent that reads through its shell
    /// turns the read-before-edit gate off).
    fn file_tools(&self) -> nori_tools::WorkspaceConfig {
        nori_tools::WorkspaceConfig::default()
    }

    /// Usage windows a response's headers report.
    fn rate_limits(&self, _headers: &[(String, String)]) -> Vec<RateLimitWindow> {
        Vec::new()
    }

    /// Where the engine keeps this agent's sessions.
    fn store_root(&self) -> PathBuf;

    /// The ways to sign in, offered at `initialize`.
    fn auth_methods(&self) -> Vec<AuthMethod> {
        Vec::new()
    }

    /// Sign in with `method`, one of [`Flavor::auth_methods`] (ACP `authenticate`).
    async fn authenticate(&self, _method: &str) -> Result<(), AgentError> {
        Ok(())
    }

    /// Whether [`Flavor::logout`] signs out (the ACP `logout` capability).
    fn can_logout(&self) -> bool {
        false
    }

    /// Sign out (ACP `logout`).
    async fn logout(&self) -> Result<(), AgentError> {
        Err(AgentError::Unsupported("logout"))
    }

    /// Headers that name the session on every request it sends to `endpoint` (Codex's
    /// `session_id`, OpenCode's `x-opencode-session`).
    fn session_headers(
        &self,
        _session: &str,
        _endpoint: &nori_llm::Endpoint,
    ) -> Vec<(String, String)> {
        Vec::new()
    }

    /// The provider refused the credential (HTTP 401) before answering: renew it where that is
    /// possible (a sign-in another program renewed on disk, a token refresh) and say whether the
    /// request is worth sending once more.
    async fn reauthorize(&self) -> bool {
        false
    }
}
