//! The TUI's actions: what each built-in [`nori_actions::Action`] does here.
//!
//! The registry (`nori-actions`) defines every action once; this module fills it with Nori's
//! built-in ones and maps an [`Invocation`] back to the code that carries it out. Slash commands
//! are actions named after the command; the dials (model, effort, mode) are actions that take a
//! value and can be attached to a draft as directives.

use nori_actions::Action;
use nori_actions::ActionId;
use nori_actions::Category;
use nori_actions::ChoiceSource;
use nori_actions::Invocation;
use nori_actions::Param;
use nori_actions::Registry;
use nori_actions::Requires;
use nori_actions::Situation;

use crate::chatwidget::ModelStep;
use crate::slash_command::CommandScope;
use crate::slash_command::SlashCommand;
use crate::slash_command::built_in_slash_commands;
use nori_protocol::Dial;

/// What a built-in action does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Builtin {
    Slash(SlashCommand),
    /// Set a dial to a value (`model.set opus`), or open its picker without one.
    SetDial(Dial),
    StepDial(Dial, ModelStep),
    /// Choose the editor's keymap.
    EditorKeymap,
}

const DIALS: [(Dial, &str, &str, &str); 3] = [
    (Dial::Model, "model.set", "Switch model", "model"),
    (Dial::Effort, "effort.set", "Set reasoning effort", "effort"),
    (Dial::Mode, "mode.set", "Set model mode", "mode"),
];

/// The built-in action an id names.
pub(crate) fn builtin(id: &ActionId) -> Option<Builtin> {
    let id = id.as_str();
    if let Some(name) = id.strip_prefix("command.") {
        return built_in_slash_commands()
            .into_iter()
            .find(|(command, _)| *command == name)
            .map(|(_, command)| Builtin::Slash(command));
    }
    for (dial, set, ..) in DIALS {
        if id == set {
            return Some(Builtin::SetDial(dial));
        }
        let noun = match dial {
            Dial::Model => "model",
            Dial::Effort => "effort",
            Dial::Mode => "mode",
        };
        if id == format!("{noun}.next") {
            return Some(Builtin::StepDial(dial, ModelStep::Next));
        }
        if id == format!("{noun}.previous") {
            return Some(Builtin::StepDial(dial, ModelStep::Previous));
        }
    }
    (id == "editor.keymap").then_some(Builtin::EditorKeymap)
}

/// Every built-in action, most used first.
pub(crate) fn builtin_registry(hotkeys: &nori_config::HotkeyConfig) -> Registry {
    use nori_config::HotkeyAction;
    let key = |action| hotkeys.binding_for(action).display_name();
    let mut registry = Registry::new();
    for (dial, set, title, name) in DIALS {
        let source = match dial {
            Dial::Model => ChoiceSource::Models,
            Dial::Effort => ChoiceSource::Efforts,
            Dial::Mode => ChoiceSource::Modes,
        };
        let summary = format!("choose the {name}; in a draft, applies when it is sent");
        registry.register(
            Action::new(set, title, &summary, Category::Model)
                .named(&[name])
                .param(Param::Choice(source))
                .on_send(),
        );
        let (next, previous) = match dial {
            Dial::Model => (HotkeyAction::NextModel, HotkeyAction::PreviousModel),
            Dial::Effort => (HotkeyAction::NextEffort, HotkeyAction::PreviousEffort),
            Dial::Mode => (HotkeyAction::NextModelMode, HotkeyAction::PreviousModelMode),
        };
        registry.register(
            Action::new(
                &format!("{name}.next"),
                &format!("Next {name}"),
                &format!("step to the next {name}"),
                Category::Model,
            )
            .keys(&[&key(next)]),
        );
        registry.register(
            Action::new(
                &format!("{name}.previous"),
                &format!("Previous {name}"),
                &format!("step to the previous {name}"),
                Category::Model,
            )
            .keys(&[&key(previous)]),
        );
    }
    for (name, command) in built_in_slash_commands() {
        // `/model` is the model dial's: with a value it sets it, without one it opens the picker.
        if command == SlashCommand::Model {
            continue;
        }
        registry.register(
            Action::new(
                &format!("command.{name}"),
                &{
                    let mut title = name.replace('-', " ");
                    if let Some(first) = title.get_mut(0..1) {
                        first.make_ascii_uppercase();
                    }
                    title
                },
                command.description(),
                category_of(command),
            )
            .named(&[name])
            .requires(Requires {
                idle: !command.available_during_task(),
                main: false,
                local: command.scope() == CommandScope::LocalOnly,
                cloud: command.scope() == CommandScope::CloudOnly,
            }),
        );
    }
    registry.register(
        Action::new(
            "editor.keymap",
            "Editor keys",
            "Helix, Vim or modeless editing for the prompt",
            Category::Settings,
        )
        .named(&["keys", "keymap"])
        .keywords(&["helix", "vim", "kakoune", "emacs", "editor"])
        .param(Param::Choice(ChoiceSource::Fixed(vec![
            choice("helix", "Helix", "selection first, modal"),
            choice("vim", "Vim", "modal"),
            choice("standard", "Standard", "modeless, readline keys"),
        ]))),
    );
    registry
}

fn choice(value: &str, label: &str, detail: &str) -> nori_actions::Choice {
    nori_actions::Choice {
        value: value.to_owned(),
        label: label.to_owned(),
        detail: Some(detail.to_owned()),
    }
}

fn category_of(command: SlashCommand) -> Category {
    match command {
        SlashCommand::Model | SlashCommand::Config => Category::Model,
        SlashCommand::Providers | SlashCommand::Login | SlashCommand::Logout => Category::Account,
        SlashCommand::Approvals
        | SlashCommand::Settings
        | SlashCommand::Vim
        | SlashCommand::Mcp
        | SlashCommand::SwitchSkillset => Category::Settings,
        SlashCommand::New
        | SlashCommand::Resume
        | SlashCommand::Close
        | SlashCommand::Quit
        | SlashCommand::Exit
        | SlashCommand::RemoteControl => Category::Session,
        SlashCommand::Move => Category::Machine,
        SlashCommand::Goal
        | SlashCommand::Init
        | SlashCommand::Compact
        | SlashCommand::Undo
        | SlashCommand::Continue
        | SlashCommand::Fork
        | SlashCommand::Reference
        | SlashCommand::FirstPrompt => Category::Conversation,
        SlashCommand::Status
        | SlashCommand::Context
        | SlashCommand::Usage
        | SlashCommand::Memory => Category::Context,
        SlashCommand::Browse | SlashCommand::Diff | SlashCommand::Mention => Category::View,
        SlashCommand::Browser => Category::Tools,
    }
}

/// What the TUI knows of its situation, for deciding what can run.
pub(crate) fn situation(turn_running: bool, in_child: bool, cloud: bool) -> Situation {
    Situation {
        turn_running,
        in_child,
        cloud,
    }
}

/// A directive's chip label: `⚙ model: Opus 4`, with the value as `value_label` shows it.
pub(crate) fn directive_label(
    registry: &Registry,
    invocation: &Invocation,
    value_label: &str,
) -> String {
    let name = registry
        .get(&invocation.id)
        .and_then(Action::name)
        .unwrap_or(invocation.id.as_str())
        .to_owned();
    if value_label.is_empty() {
        format!("⚙ {name}")
    } else {
        format!("⚙ {name}: {value_label}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn every_registered_action_is_carried_out_here() {
        // `/model` is reached through the model dial.
        assert_eq!(
            builtin(&ActionId::new("command.model")),
            Some(Builtin::Slash(SlashCommand::Model))
        );
        let registry = builtin_registry(&nori_config::HotkeyConfig::default());
        let unhandled: Vec<&str> = registry
            .actions()
            .iter()
            .filter(|action| builtin(&action.id).is_none())
            .map(|action| action.id.as_str())
            .collect();
        assert_eq!(unhandled, Vec::<&str>::new());
    }

    #[test]
    fn typed_slash_commands_are_actions() {
        let registry = builtin_registry(&nori_config::HotkeyConfig::default());
        let invocation = registry.parse_command("/model opus").expect("model");
        assert_eq!(builtin(&invocation.id), Some(Builtin::SetDial(Dial::Model)));
        assert_eq!(invocation.arg.as_deref(), Some("opus"));
        let new = registry.parse_command("/new").expect("new");
        assert_eq!(builtin(&new.id), Some(Builtin::Slash(SlashCommand::New)));
    }
}
