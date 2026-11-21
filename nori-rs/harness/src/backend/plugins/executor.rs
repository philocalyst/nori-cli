//! Running a command: settle its arguments, fill them into its operations, then run the
//! operations in order, each through the session ([`PluginHost`]), until one fails.

use std::collections::BTreeMap;

use nori_pkl::kinds::Operation;

use super::arguments;
use super::host::PluginHost;
use super::types::PluginError;
use super::types::PluginRun;
use super::types::StepResult;

/// Run command `command_name` of plugin `plugin` on `host` with `arguments`.
///
/// Everything that can be known before the first operation is checked first (the plugin, the
/// command, every argument, every `{{parameter}}`), so a mistake never leaves half a command done.
/// The first operation that fails stops the command; the error names it and carries what ran
/// before it.
pub(crate) async fn run<H: PluginHost>(
    host: &H,
    plugin: &str,
    command_name: &str,
    given: BTreeMap<String, String>,
) -> Result<PluginRun, PluginError> {
    let catalog = host.modules().await;
    let module = catalog.plugin(plugin).ok_or_else(|| {
        let known: Vec<&str> = catalog
            .plugins
            .iter()
            .map(|plugin| plugin.name.as_str())
            .collect();
        PluginError::UnknownPlugin {
            plugin: plugin.to_owned(),
            known: if known.is_empty() {
                "none are loaded".to_owned()
            } else {
                known.join(", ")
            },
        }
    })?;
    let command = module.value.commands.get(command_name).ok_or_else(|| {
        let known: Vec<&str> = module.value.commands.keys().map(String::as_str).collect();
        PluginError::UnknownCommand {
            plugin: plugin.to_owned(),
            command: command_name.to_owned(),
            known: if known.is_empty() {
                "none".to_owned()
            } else {
                known.join(", ")
            },
        }
    })?;
    let values = arguments::settle(plugin, command_name, command, given)?;
    let operations = arguments::fill(command_name, &command.operations, &values)?;

    let mut steps = Vec::with_capacity(operations.len());
    for (index, operation) in operations.into_iter().enumerate() {
        let at = arguments::place(command_name, index, &operation);
        match step(host, operation).await {
            Ok(result) => steps.push(result),
            Err(message) => {
                return Err(PluginError::StepFailed {
                    at,
                    message,
                    completed: steps,
                });
            }
        }
    }
    Ok(PluginRun {
        plugin: plugin.to_owned(),
        command: command_name.to_owned(),
        steps,
    })
}

/// One operation, as the session does it.
async fn step<H: PluginHost>(host: &H, operation: Operation) -> Result<StepResult, String> {
    Ok(match operation {
        Operation::SessionDescribe {} => StepResult::SessionDescribed {
            description: Box::new(host.describe().await),
        },
        Operation::SessionExport { path } => {
            let exported = host.export(path.map(Into::into)).await?;
            StepResult::SessionExported {
                path: exported.path,
                conversation_id: exported.conversation_id,
                bytes: exported.bytes,
            }
        }
        Operation::SessionMove { machine } => {
            if machine.trim().is_empty() {
                return Err("the machine is empty".to_owned());
            }
            host.request_move(&machine).await?;
            StepResult::SessionMoveRequested { machine }
        }
        Operation::JobStart { job } => {
            let started = host.start_job(&job).await?;
            StepResult::JobStarted {
                job: started.job,
                label: started.label,
                state: started.state,
            }
        }
        Operation::JobStop { job } => {
            let stopped = host.stop_job(&job).await?;
            StepResult::JobStopped {
                job: stopped.job,
                label: stopped.label,
                state: stopped.state,
            }
        }
        Operation::MonitorStart { monitor } => StepResult::MonitorStarted {
            armed: host.start_monitor(&monitor).await?,
            monitor,
        },
        Operation::MonitorStop { monitor } => StepResult::MonitorStopped {
            stopped: host.stop_monitor(&monitor).await?,
            monitor,
        },
        Operation::SkillRun { skill, arguments } => {
            let outcome = host.run_skill(&skill, arguments).await?;
            StepResult::SkillRan {
                skill,
                journal: outcome.journal,
                value: outcome.value,
            }
        }
        Operation::Notify { title, body } => StepResult::Notified {
            headline: host.notify(title, body).await,
        },
        Operation::Prompt { text } => {
            if text.trim().is_empty() {
                return Err("the prompt is empty once its arguments are filled in".to_owned());
            }
            host.queue_prompt(text.clone()).await?;
            StepResult::PromptQueued { text }
        }
        Operation::ConfigSet { key, value } => {
            host.set_config(&key, &value).await?;
            StepResult::ConfigSet { key, value }
        }
    })
}
