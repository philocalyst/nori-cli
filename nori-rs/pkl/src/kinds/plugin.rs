//! The `nori:Plugin` kind: commands built from typed operations on Nori itself.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::LazyLock;

use regex::Regex;
use serde::Deserialize;
use serde::Serialize;

use super::check_name;
use super::check_not_empty;
use super::finish;

/// One argument of a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Parameter {
    /// What the argument is.
    pub description: String,
    /// The value used when the caller gives none.
    #[serde(default)]
    pub default: Option<String>,
    /// Whether the caller must give a value.
    pub required: bool,
    /// The values the argument may take.
    #[serde(default)]
    pub choices: Vec<String>,
    /// Where the frontend gets the choices from.
    #[serde(default)]
    pub from: Option<ChoiceFrom>,
}

/// Where the frontend lists an argument's choices from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChoiceFrom {
    Machines,
    Models,
    Efforts,
    Modes,
    Agents,
    Skills,
    Files,
    Sessions,
}

/// What part of Nori a command is about, for grouping in the palette.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandCategory {
    Conversation,
    Session,
    Model,
    Agent,
    Machine,
    Context,
    View,
    Settings,
    Account,
    Tools,
    #[default]
    Plugin,
}

/// When a command takes effect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandEffect {
    /// When it is chosen.
    #[default]
    Now,
    /// When the prompt it is attached to is sent.
    OnSend,
}

/// What must hold for a command to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandRequires {
    /// No turn may be running.
    #[serde(default)]
    pub idle: bool,
    /// Only in the main conversation.
    #[serde(default)]
    pub main: bool,
    /// Only for a session on this machine.
    #[serde(default)]
    pub local: bool,
    /// Only for a cloud session.
    #[serde(default)]
    pub cloud: bool,
}

/// One step of a command: something Nori does to itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Operation {
    /// Report what the session is.
    #[serde(rename = "session.describe")]
    SessionDescribe {},
    /// Bundle the session into a file.
    #[serde(rename = "session.export")]
    SessionExport {
        /// Where to write the bundle.
        #[serde(default)]
        path: Option<String>,
    },
    /// Move the session to another machine.
    #[serde(rename = "session.move")]
    SessionMove {
        /// The target machine.
        machine: String,
    },
    /// Start a job module by name.
    #[serde(rename = "job.start")]
    JobStart {
        /// The job's name.
        job: String,
    },
    /// Stop a running job.
    #[serde(rename = "job.stop")]
    JobStop {
        /// The job's name.
        job: String,
    },
    /// Arm a monitor module by name.
    #[serde(rename = "monitor.start")]
    MonitorStart {
        /// The monitor's name.
        monitor: String,
    },
    /// Disarm a monitor.
    #[serde(rename = "monitor.stop")]
    MonitorStop {
        /// The monitor's name.
        monitor: String,
    },
    /// Run a skill.
    #[serde(rename = "skill.run")]
    SkillRun {
        /// The skill's name.
        skill: String,
        /// Values for the skill's parameters.
        arguments: BTreeMap<String, String>,
    },
    /// Tell the person.
    #[serde(rename = "notify")]
    Notify {
        /// The notification's title.
        #[serde(default)]
        title: Option<String>,
        /// The notification's text.
        #[serde(default)]
        body: Option<String>,
    },
    /// Queue a prompt for the agent.
    #[serde(rename = "prompt")]
    Prompt {
        /// The prompt's text.
        text: String,
    },
    /// Change a configuration option.
    #[serde(rename = "config.set")]
    ConfigSet {
        /// The option's key.
        key: String,
        /// The new value.
        value: String,
    },
}

/// A named action of a plugin.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// What the command does.
    pub description: String,
    /// What the palette lists it as; the description when unset.
    #[serde(default)]
    pub title: Option<String>,
    /// What a person types after `/`; `<plugin>-<command>` when empty.
    #[serde(default)]
    pub names: Vec<String>,
    /// More words the palette finds it by.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// The palette group.
    #[serde(default)]
    pub category: CommandCategory,
    /// When it takes effect.
    #[serde(default)]
    pub effect: CommandEffect,
    /// What must hold for it to run.
    #[serde(default)]
    pub requires: CommandRequires,
    /// The command's arguments by name.
    #[serde(default)]
    pub parameters: BTreeMap<String, Parameter>,
    /// What it does, in order.
    #[serde(default)]
    pub operations: Vec<Operation>,
}

/// A plugin module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginDef {
    /// The plugin's name (`[a-z0-9][a-z0-9_-]*`).
    pub name: String,
    /// One line saying what the plugin is for.
    pub description: String,
    /// The plugin's commands by name.
    #[serde(default)]
    pub commands: BTreeMap<String, Command>,
}

static TEMPLATE: LazyLock<Regex> = LazyLock::new(|| {
    // The pattern is a constant that compiles; a failure here is a bug caught by every test.
    Regex::new(r"\{\{\s*([^{}\s]+)\s*\}\}").unwrap_or_else(|error| panic!("{error}"))
});

impl PluginDef {
    /// Checks what the types cannot: the names, and that every `{{param}}` in an operation names
    /// a parameter its command declares. Every message names the field.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        check_name("name", &self.name, &mut problems);
        for (command_name, command) in &self.commands {
            let command_at = format!("commands[{command_name:?}]");
            check_name(
                &format!("{command_at} (command name)"),
                command_name,
                &mut problems,
            );
            if let Some(title) = &command.title {
                check_not_empty(&format!("{command_at}.title"), title, &mut problems);
            }
            for (index, name) in command.names.iter().enumerate() {
                check_name(&format!("{command_at}.names[{index}]"), name, &mut problems);
            }
            for (index, keyword) in command.keywords.iter().enumerate() {
                check_not_empty(
                    &format!("{command_at}.keywords[{index}]"),
                    keyword,
                    &mut problems,
                );
            }
            for (parameter, declared) in &command.parameters {
                let valid = !parameter.is_empty()
                    && parameter
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_');
                if !valid {
                    problems.push(format!(
                        "{command_at}.parameters: {parameter:?} is not a valid parameter name (letters, digits and `_`)"
                    ));
                }
                let at = format!("{command_at}.parameters[{parameter:?}]");
                if !declared.choices.is_empty() && declared.from.is_some() {
                    problems.push(format!("{at}: give `choices` or `from`, not both"));
                }
                let mut seen = BTreeSet::new();
                for (index, choice) in declared.choices.iter().enumerate() {
                    check_not_empty(&format!("{at}.choices[{index}]"), choice, &mut problems);
                    if !seen.insert(choice.as_str()) {
                        problems.push(format!("{at}.choices[{index}]: {choice:?} is listed twice"));
                    }
                }
                if let Some(default) = &declared.default
                    && !declared.choices.is_empty()
                    && !declared.choices.contains(default)
                {
                    problems.push(format!(
                        "{at}.default: {default:?} is not one of the choices ({})",
                        declared.choices.join(", ")
                    ));
                }
            }
            let declared: BTreeSet<&str> = command.parameters.keys().map(String::as_str).collect();
            for (index, operation) in command.operations.iter().enumerate() {
                let at = format!("{command_at}.operations[{index}]");
                let fields: Vec<(&str, &str)> = match operation {
                    Operation::SessionDescribe {} => Vec::new(),
                    Operation::SessionExport { path } => {
                        path.iter().map(|path| ("path", path.as_str())).collect()
                    }
                    Operation::SessionMove { machine } => vec![("machine", machine)],
                    Operation::JobStart { job } | Operation::JobStop { job } => {
                        vec![("job", job)]
                    }
                    Operation::MonitorStart { monitor } | Operation::MonitorStop { monitor } => {
                        vec![("monitor", monitor)]
                    }
                    Operation::SkillRun { skill, arguments } => {
                        let mut fields = vec![("skill", skill.as_str())];
                        fields.extend(
                            arguments
                                .values()
                                .map(|value| ("arguments", value.as_str())),
                        );
                        fields
                    }
                    Operation::Notify { title, body } => title
                        .iter()
                        .map(|title| ("title", title.as_str()))
                        .chain(body.iter().map(|body| ("body", body.as_str())))
                        .collect(),
                    Operation::Prompt { text } => vec![("text", text)],
                    Operation::ConfigSet { key, value } => vec![("key", key), ("value", value)],
                };
                for (field, text) in &fields {
                    for capture in TEMPLATE.captures_iter(text) {
                        let name = &capture[1];
                        let root = name.split('.').next().unwrap_or(name);
                        if !declared.contains(root) {
                            let known = if declared.is_empty() {
                                "the command declares none".to_owned()
                            } else {
                                format!(
                                    "declared: {}",
                                    declared.iter().copied().collect::<Vec<_>>().join(", ")
                                )
                            };
                            problems.push(format!(
                                "{at}.{field}: `{{{{{name}}}}}` is not a declared parameter ({known})"
                            ));
                        }
                    }
                }
                match operation {
                    Operation::SessionMove { machine } => {
                        check_not_empty(&format!("{at}.machine"), machine, &mut problems);
                    }
                    Operation::JobStart { job } | Operation::JobStop { job } => {
                        check_not_empty(&format!("{at}.job"), job, &mut problems);
                    }
                    Operation::MonitorStart { monitor } | Operation::MonitorStop { monitor } => {
                        check_not_empty(&format!("{at}.monitor"), monitor, &mut problems);
                    }
                    Operation::SkillRun { skill, .. } => {
                        check_not_empty(&format!("{at}.skill"), skill, &mut problems);
                    }
                    Operation::Prompt { text } => {
                        check_not_empty(&format!("{at}.text"), text, &mut problems);
                    }
                    Operation::ConfigSet { key, .. } => {
                        check_not_empty(&format!("{at}.key"), key, &mut problems);
                    }
                    Operation::SessionDescribe {}
                    | Operation::SessionExport { .. }
                    | Operation::Notify { .. } => {}
                }
            }
        }
        finish(problems)
    }
}
