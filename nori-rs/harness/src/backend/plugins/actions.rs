//! A plugin's commands as the action registry's actions (`nori-actions`): what the TUI's palette
//! lists and `/name value` runs. The projection is a pure function of the catalog; the feed keeps
//! the session's view of it current as modules (re)load.

use std::sync::Arc;

use nori_actions::Action;
use nori_actions::ActionId;
use nori_actions::Category;
use nori_actions::Choice;
use nori_actions::ChoiceSource;
use nori_actions::Effect;
use nori_actions::Param;
use nori_actions::Requires;
use nori_actions::Source;
use nori_pkl::catalog::Catalog;
use nori_pkl::kinds::ChoiceFrom;
use nori_pkl::kinds::Command;
use nori_pkl::kinds::CommandCategory;
use nori_pkl::kinds::CommandEffect;
use tokio::sync::watch;

use crate::backend::AcpBackend;

/// One action per command of every plugin, in plugin and command order.
pub(crate) fn project(catalog: &Catalog) -> Arc<[Action]> {
    catalog
        .plugins
        .iter()
        .flat_map(|plugin| {
            plugin
                .value
                .commands
                .iter()
                .map(|(name, command)| action(&plugin.name, name, command))
        })
        .collect()
}

fn action(plugin: &str, command_name: &str, command: &Command) -> Action {
    let id = format!("{plugin}.{command_name}");
    let title = match command.title.as_deref() {
        Some(title) => title.to_owned(),
        None if command.description.is_empty() => id.clone(),
        None => command.description.clone(),
    };
    Action {
        id: ActionId::new(id),
        title,
        summary: command.description.clone(),
        names: if command.names.is_empty() {
            vec![format!("{plugin}-{command_name}")]
        } else {
            command.names.clone()
        },
        keywords: command.keywords.clone(),
        category: match command.category {
            CommandCategory::Conversation => Category::Conversation,
            CommandCategory::Session => Category::Session,
            CommandCategory::Model => Category::Model,
            CommandCategory::Agent => Category::Agent,
            CommandCategory::Machine => Category::Machine,
            CommandCategory::Context => Category::Context,
            CommandCategory::View => Category::View,
            CommandCategory::Settings => Category::Settings,
            CommandCategory::Account => Category::Account,
            CommandCategory::Tools => Category::Tools,
            CommandCategory::Plugin => Category::Plugin,
        },
        param: param(command),
        effect: match command.effect {
            CommandEffect::Now => Effect::Now,
            CommandEffect::OnSend => Effect::OnSend,
        },
        requires: Requires {
            idle: command.requires.idle,
            main: command.requires.main,
            local: command.requires.local,
            cloud: command.requires.cloud,
        },
        keys: Vec::new(),
        source: Source::Plugin(plugin.to_owned()),
    }
}

/// What the action takes: nothing; one choice or one text for a single parameter; free text for
/// several, which the invocation reads as `name=value` pairs (see `invocation`).
fn param(command: &Command) -> Param {
    let parameters: Vec<_> = command.parameters.iter().collect();
    match parameters.as_slice() {
        [] => Param::None,
        [(name, parameter)] => {
            if !parameter.choices.is_empty() {
                Param::Choice(ChoiceSource::Fixed(
                    parameter
                        .choices
                        .iter()
                        .map(|choice| Choice {
                            value: choice.clone(),
                            label: choice.clone(),
                            detail: None,
                        })
                        .collect(),
                ))
            } else if let Some(from) = parameter.from {
                Param::Choice(match from {
                    ChoiceFrom::Machines => ChoiceSource::Machines,
                    ChoiceFrom::Models => ChoiceSource::Models,
                    ChoiceFrom::Efforts => ChoiceSource::Efforts,
                    ChoiceFrom::Modes => ChoiceSource::Modes,
                    ChoiceFrom::Agents => ChoiceSource::Agents,
                    ChoiceFrom::Skills => ChoiceSource::Skills,
                    ChoiceFrom::Files => ChoiceSource::Files,
                    ChoiceFrom::Sessions => ChoiceSource::Sessions,
                })
            } else {
                Param::Text {
                    placeholder: if parameter.description.is_empty() {
                        (*name).clone()
                    } else {
                        parameter.description.clone()
                    },
                    required: parameter.required && parameter.default.is_none(),
                }
            }
        }
        [_, _, ..] => Param::Text {
            placeholder: parameters
                .iter()
                .map(|(name, _)| format!("{name}=…"))
                .collect::<Vec<_>>()
                .join(" "),
            required: parameters
                .iter()
                .any(|(_, parameter)| parameter.required && parameter.default.is_none()),
        },
    }
}

/// The session's plugin actions, held where a frontend can watch them. The handle owns one and
/// every backend the session has follows its modules into it.
#[derive(Clone)]
pub(crate) struct PluginActionsFeed(Arc<watch::Sender<Arc<[Action]>>>);

impl Default for PluginActionsFeed {
    fn default() -> Self {
        Self(Arc::new(watch::channel(Arc::from(Vec::new())).0))
    }
}

impl PluginActionsFeed {
    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<[Action]>> {
        self.0.subscribe()
    }

    /// Publish the actions of `backend`'s modules now and whenever they load again, until the
    /// backend is gone. A session that swaps its backend follows the new one the same way.
    pub(crate) fn follow(&self, backend: &AcpBackend) {
        let feed = Arc::clone(&self.0);
        let mut modules = backend.orchestrator.module_updates();
        tokio::spawn(async move {
            loop {
                let loaded = modules.borrow_and_update().clone();
                if let Some(catalog) = loaded {
                    feed.send_replace(project(&catalog));
                }
                if modules.changed().await.is_err() {
                    break;
                }
            }
        });
    }
}
