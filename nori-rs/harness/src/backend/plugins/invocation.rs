//! An action invocation (`nori_actions::Invocation`) as a plugin command run: which command the
//! id names, and the argument read as the command's parameters.
//!
//! A command with no parameters takes no argument. With one parameter the whole argument is its
//! value, spaces and all. With several the argument is `name=value` pairs, split like a shell
//! splits words (`note="two words"`), and a bare value fills the first required parameter that has
//! no value yet, so `/rex-move gpu` works for a command whose other parameters are optional.

use std::collections::BTreeMap;

use nori_actions::Invocation;

use super::types::CommandInfo;
use super::types::ParameterInfo;
use super::types::PluginError;
use super::types::PluginInfo;

/// The plugin, command and arguments `invocation` runs, or why it cannot run.
pub(crate) fn resolve(
    plugins: &[PluginInfo],
    invocation: &Invocation,
) -> Result<(String, String, BTreeMap<String, String>), PluginError> {
    let found = invocation
        .id
        .as_str()
        .split_once('.')
        .and_then(|(plugin, command)| {
            plugins
                .iter()
                .find(|candidate| candidate.name == plugin)
                .and_then(|plugin| {
                    plugin
                        .commands
                        .iter()
                        .find(|candidate| candidate.name == command)
                        .map(|command| (plugin, command))
                })
        });
    let Some((plugin, command)) = found else {
        let known: Vec<String> = plugins
            .iter()
            .flat_map(|plugin| {
                plugin
                    .commands
                    .iter()
                    .map(|command| format!("{}.{}", plugin.name, command.name))
            })
            .collect();
        return Err(PluginError::UnknownAction {
            action: invocation.id.to_string(),
            known: if known.is_empty() {
                "none are loaded".to_owned()
            } else {
                known.join(", ")
            },
        });
    };
    let arguments = arguments(&plugin.name, command, invocation.arg.as_deref())?;
    Ok((plugin.name.clone(), command.name.clone(), arguments))
}

fn arguments(
    plugin: &str,
    command: &CommandInfo,
    arg: Option<&str>,
) -> Result<BTreeMap<String, String>, PluginError> {
    let bad = |message: String| PluginError::BadArgument {
        plugin: plugin.to_owned(),
        command: command.name.clone(),
        message,
    };
    let Some(arg) = arg.map(str::trim).filter(|arg| !arg.is_empty()) else {
        return Ok(BTreeMap::new());
    };
    let (first, rest) = match command.parameters.split_first() {
        None => return Err(bad(format!("it takes no argument, but was given `{arg}`"))),
        Some(split) => split,
    };
    if rest.is_empty() {
        return Ok(BTreeMap::from([(first.name.clone(), arg.to_owned())]));
    }
    let words = shlex::split(arg).ok_or_else(|| bad(format!("`{arg}` has an unclosed quote")))?;
    let mut values = BTreeMap::new();
    let mut bare = Vec::new();
    for word in words {
        match word.split_once('=') {
            Some((name, value)) if is_name(name) => {
                if !command.parameters.iter().any(|known| known.name == name) {
                    return Err(PluginError::UnknownArgument {
                        plugin: plugin.to_owned(),
                        command: command.name.clone(),
                        name: name.to_owned(),
                        known: command
                            .parameters
                            .iter()
                            .map(|known| known.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
                if values.insert(name.to_owned(), value.to_owned()).is_some() {
                    return Err(bad(format!("`{name}` is given twice")));
                }
            }
            Some(_) | None => bare.push(word),
        }
    }
    // Bare values go to the required parameters that the pairs did not fill, in name order.
    for word in bare {
        let Some(parameter) = command
            .parameters
            .iter()
            .find(|parameter| is_open(parameter) && !values.contains_key(&parameter.name))
        else {
            return Err(bad(format!("no required parameter is left for `{word}`")));
        };
        values.insert(parameter.name.clone(), word);
    }
    Ok(values)
}

/// A parameter the caller has to give: required, and nothing stands in for it.
fn is_open(parameter: &ParameterInfo) -> bool {
    parameter.required && parameter.default.is_none()
}

fn is_name(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}
