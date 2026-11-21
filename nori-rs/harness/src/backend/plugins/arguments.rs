//! A command's arguments: checked against what it declares, then filled into every text field of
//! its operations. Nothing runs until all of it is settled, so a typo never leaves half a command
//! done.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use nori_pkl::kinds::Command;
use nori_pkl::kinds::Operation;
use regex::Regex;

use super::types::PluginError;

static PLACEHOLDER: LazyLock<Regex> = LazyLock::new(|| {
    // The pattern is a constant that compiles; a failure here is a bug every test catches.
    Regex::new(r"\{\{\s*([^{}\s]+)\s*\}\}").unwrap_or_else(|error| panic!("{error}"))
});

/// The values every parameter of `command` has for this run: given, else its default. An argument
/// the command does not declare, and a required parameter nobody gave, are errors naming them.
pub(super) fn settle(
    plugin: &str,
    command_name: &str,
    command: &Command,
    mut given: BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, PluginError> {
    let declared: Vec<&str> = command.parameters.keys().map(String::as_str).collect();
    if let Some(name) = given
        .keys()
        .find(|name| !command.parameters.contains_key(*name))
    {
        return Err(PluginError::UnknownArgument {
            plugin: plugin.to_owned(),
            command: command_name.to_owned(),
            name: name.clone(),
            known: if declared.is_empty() {
                "none".to_owned()
            } else {
                declared.join(", ")
            },
        });
    }
    let mut values = BTreeMap::new();
    for (name, parameter) in &command.parameters {
        let value = match (given.remove(name), &parameter.default) {
            (Some(value), _) => value,
            (None, Some(default)) => default.clone(),
            (None, None) if parameter.required => {
                return Err(PluginError::MissingArgument {
                    plugin: plugin.to_owned(),
                    command: command_name.to_owned(),
                    name: name.clone(),
                });
            }
            (None, None) => String::new(),
        };
        values.insert(name.clone(), value);
    }
    Ok(values)
}

/// Where operation `index` of `command` is, for an error: `commands.to.operations[0] (session.move)`.
pub(super) fn place(command: &str, index: usize, operation: &Operation) -> String {
    let kind = match operation {
        Operation::SessionDescribe {} => "session.describe",
        Operation::SessionExport { .. } => "session.export",
        Operation::SessionMove { .. } => "session.move",
        Operation::JobStart { .. } => "job.start",
        Operation::JobStop { .. } => "job.stop",
        Operation::MonitorStart { .. } => "monitor.start",
        Operation::MonitorStop { .. } => "monitor.stop",
        Operation::SkillRun { .. } => "skill.run",
        Operation::Notify { .. } => "notify",
        Operation::Prompt { .. } => "prompt",
        Operation::ConfigSet { .. } => "config.set",
    };
    format!("commands.{command}.operations[{index}] ({kind})")
}

/// Every operation of the command with its `{{parameter}}`s replaced by `values`.
pub(super) fn fill(
    command_name: &str,
    operations: &[Operation],
    values: &BTreeMap<String, String>,
) -> Result<Vec<Operation>, PluginError> {
    operations
        .iter()
        .enumerate()
        .map(|(index, operation)| {
            let at = place(command_name, index, operation);
            let text = |text: &str| render(&at, text, values);
            let optional = |value: &Option<String>| value.as_deref().map(&text).transpose();
            Ok(match operation {
                Operation::SessionDescribe {} => Operation::SessionDescribe {},
                Operation::SessionExport { path } => Operation::SessionExport {
                    path: optional(path)?,
                },
                Operation::SessionMove { machine } => Operation::SessionMove {
                    machine: text(machine)?,
                },
                Operation::JobStart { job } => Operation::JobStart { job: text(job)? },
                Operation::JobStop { job } => Operation::JobStop { job: text(job)? },
                Operation::MonitorStart { monitor } => Operation::MonitorStart {
                    monitor: text(monitor)?,
                },
                Operation::MonitorStop { monitor } => Operation::MonitorStop {
                    monitor: text(monitor)?,
                },
                Operation::SkillRun { skill, arguments } => Operation::SkillRun {
                    skill: text(skill)?,
                    arguments: arguments
                        .iter()
                        .map(|(name, value)| Ok((name.clone(), text(value)?)))
                        .collect::<Result<_, PluginError>>()?,
                },
                Operation::Notify { title, body } => Operation::Notify {
                    title: optional(title)?,
                    body: optional(body)?,
                },
                Operation::Prompt { text: prompt } => Operation::Prompt {
                    text: text(prompt)?,
                },
                Operation::ConfigSet { key, value } => Operation::ConfigSet {
                    key: text(key)?,
                    value: text(value)?,
                },
            })
        })
        .collect()
}

/// `text` with each `{{name}}` replaced by its value; a name that is not an argument is an error.
fn render(at: &str, text: &str, values: &BTreeMap<String, String>) -> Result<String, PluginError> {
    let mut rendered = String::with_capacity(text.len());
    let mut rest = 0;
    for capture in PLACEHOLDER.captures_iter(text) {
        let (whole, name) = (&capture[0], &capture[1]);
        let Some(value) = values.get(name) else {
            return Err(PluginError::UnknownPlaceholder {
                at: at.to_owned(),
                name: name.to_owned(),
            });
        };
        let start = capture.get(0).map_or(rest, |found| found.start());
        rendered.push_str(&text[rest..start]);
        rendered.push_str(value);
        rest = start + whole.len();
    }
    rendered.push_str(&text[rest..]);
    Ok(rendered)
}
