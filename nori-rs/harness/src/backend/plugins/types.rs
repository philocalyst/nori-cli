//! What the plugin runtime says: the plugins a session offers, what a run did step by step, and
//! why a run stopped. All of it is serde, because a person's frontend and a Rex client read it
//! over the remote-control channel as they read it in process.

use std::path::PathBuf;

use nori_config::AskForApproval;
use nori_config::SandboxPolicy;
use nori_pkl::catalog::Catalog;
use serde::Deserialize;
use serde::Serialize;

/// A plugin a session offers: what its commands are called and take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub description: String,
    /// In name order.
    pub commands: Vec<CommandInfo>,
}

/// One command of a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandInfo {
    pub name: String,
    pub description: String,
    /// In name order.
    pub parameters: Vec<ParameterInfo>,
}

/// One argument of a command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterInfo {
    pub name: String,
    pub description: String,
    /// The caller must give it: it is required and has no default.
    pub required: bool,
    pub default: Option<String>,
}

impl PluginInfo {
    /// The plugins the catalog loaded.
    pub fn of(catalog: &Catalog) -> Vec<Self> {
        catalog
            .plugins
            .iter()
            .map(|plugin| Self {
                name: plugin.name.clone(),
                description: plugin.value.description.clone(),
                commands: plugin
                    .value
                    .commands
                    .iter()
                    .map(|(name, command)| CommandInfo {
                        name: name.clone(),
                        description: command.description.clone(),
                        parameters: command
                            .parameters
                            .iter()
                            .map(|(name, parameter)| ParameterInfo {
                                name: name.clone(),
                                description: parameter.description.clone(),
                                required: parameter.required && parameter.default.is_none(),
                                default: parameter.default.clone(),
                            })
                            .collect(),
                    })
                    .collect(),
            })
            .collect()
    }
}

/// What a session is, as `session.describe` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDescription {
    pub agent: String,
    pub conversation_id: String,
    pub cwd: PathBuf,
    /// This computer's name.
    pub machine: String,
    /// The agent's session settings: model, effort, mode and whatever else it offers.
    pub settings: Vec<SessionSetting>,
    pub jobs: Vec<JobDescription>,
    pub monitors: Vec<MonitorDescription>,
    pub permissions: PermissionDescription,
}

/// One session setting and the value it has now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSetting {
    pub id: String,
    pub name: String,
    pub value: String,
}

/// A job the session follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobDescription {
    pub label: String,
    pub command: String,
    /// The machine it runs on.
    pub machine: String,
    pub state: String,
}

/// A monitor of the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDescription {
    pub label: String,
    pub watching: String,
    pub state: String,
}

/// The rules that decide what the agent may do without asking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionDescription {
    pub approval_policy: AskForApproval,
    pub sandbox: SandboxPolicy,
}

/// A command that ran to its end: one result per operation, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRun {
    pub plugin: String,
    pub command: String,
    pub steps: Vec<StepResult>,
}

/// What one operation did. The `kind` is the operation's name in the plugin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum StepResult {
    #[serde(rename = "session.describe")]
    SessionDescribed {
        description: Box<SessionDescription>,
    },
    #[serde(rename = "session.export")]
    SessionExported {
        path: PathBuf,
        conversation_id: String,
        bytes: i64,
    },
    /// The frontend was asked to move the session; it shows the move's own lines.
    #[serde(rename = "session.move")]
    SessionMoveRequested { machine: String },
    #[serde(rename = "job.start")]
    JobStarted {
        job: String,
        label: String,
        state: String,
    },
    #[serde(rename = "job.stop")]
    JobStopped {
        job: String,
        label: String,
        state: String,
    },
    #[serde(rename = "monitor.start")]
    MonitorStarted { monitor: String, armed: Vec<String> },
    #[serde(rename = "monitor.stop")]
    MonitorStopped {
        monitor: String,
        stopped: Vec<String>,
    },
    #[serde(rename = "skill.run")]
    SkillRan {
        skill: String,
        journal: String,
        value: serde_json::Value,
    },
    #[serde(rename = "notify")]
    Notified { headline: String },
    /// The prompt is in the session's queue; the agent answers it in its turn.
    #[serde(rename = "prompt")]
    PromptQueued { text: String },
    #[serde(rename = "config.set")]
    ConfigSet { key: String, value: String },
}

/// Why a command did not run, or did not run to its end.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "error")]
pub enum PluginError {
    #[error("there is no plugin named `{plugin}`; the plugins are: {known}")]
    UnknownPlugin { plugin: String, known: String },
    #[error("plugin `{plugin}` has no command `{command}`; its commands are: {known}")]
    UnknownCommand {
        plugin: String,
        command: String,
        known: String,
    },
    #[error("`{plugin} {command}` has no argument `{name}`; its arguments are: {known}")]
    UnknownArgument {
        plugin: String,
        command: String,
        name: String,
        known: String,
    },
    #[error("`{plugin} {command}` needs the argument `{name}`")]
    MissingArgument {
        plugin: String,
        command: String,
        name: String,
    },
    #[error("{at}: `{{{{{name}}}}}` is not an argument of the command")]
    UnknownPlaceholder { at: String, name: String },
    #[error("there is no plugin action `{action}`; the plugin actions are: {known}")]
    UnknownAction { action: String, known: String },
    /// The argument of an action invocation does not fit the command's parameters.
    #[error("`{plugin} {command}`: {message}")]
    BadArgument {
        plugin: String,
        command: String,
        message: String,
    },
    #[error("the session is not running")]
    SessionClosed,
    /// The operation at `at` failed, and nothing after it ran. `completed` is what ran before.
    #[error("{at}: {message}")]
    StepFailed {
        at: String,
        message: String,
        completed: Vec<StepResult>,
    },
}
