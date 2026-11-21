//! Carrying out actions (`nori-actions`): from the palette, typed commands and directives.

use nori_actions::Choice;
use nori_actions::Invocation;
use nori_editor::KeymapKind;
use nori_protocol::Dial;

use super::ChatWidget;
use crate::actions::Builtin;
use crate::bottom_pane::ActionChoices;
use crate::slash_command::SlashCommand;

impl ChatWidget {
    /// Do what `invocation` asks.
    pub(crate) fn run_action(&mut self, invocation: Invocation) {
        let Some(builtin) = crate::actions::builtin(&invocation.id) else {
            match (
                self.bottom_pane.action_source(&invocation.id),
                self.harness_handle(),
            ) {
                (Some(nori_actions::Source::Plugin(_)), Some(handle)) => {
                    // The plugin runs on the runtime; its run reports itself in the transcript.
                    let tx = self.app_event_tx.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle.run_plugin_action(invocation).await {
                            tx.send(crate::app_event::AppEvent::TransientNotice(
                                error.to_string(),
                            ));
                        }
                    });
                }
                (
                    Some(
                        nori_actions::Source::Plugin(_)
                        | nori_actions::Source::BuiltIn
                        | nori_actions::Source::Agent
                        | nori_actions::Source::Prompt,
                    )
                    | None,
                    _,
                ) => {
                    self.add_info_message(
                        format!("Nothing carries out `{}` here.", invocation.id),
                        None,
                    );
                }
            }
            return;
        };
        match (builtin, invocation.arg) {
            (Builtin::Slash(command), _) => self.dispatch_command(command),
            (Builtin::SetDial(dial), Some(value)) => {
                if !self.set_dial(dial, &value) {
                    self.add_info_message(
                        format!(
                            "The agent has not listed its {} options yet; try again in a moment.",
                            dial.noun()
                        ),
                        None,
                    );
                }
            }
            (Builtin::SetDial(Dial::Model), None) => self.dispatch_command(SlashCommand::Model),
            (Builtin::SetDial(Dial::Effort | Dial::Mode), None) => {
                self.dispatch_command(SlashCommand::Config);
            }
            (Builtin::StepDial(Dial::Model, step), _) => {
                self.cycle_model(step);
            }
            (Builtin::StepDial(Dial::Effort, step), _) => {
                self.cycle_effort(step);
            }
            (Builtin::StepDial(Dial::Mode, step), _) => {
                self.cycle_model_mode(step);
            }
            (Builtin::EditorKeymap, Some(keymap)) => {
                let keymap = match keymap.as_str() {
                    "helix" | "kakoune" => KeymapKind::Helix,
                    "vim" => KeymapKind::Vim,
                    _ => KeymapKind::Standard,
                };
                self.bottom_pane.set_editor_keymap(keymap);
                self.request_redraw();
            }
            (Builtin::EditorKeymap, None) => self.dispatch_command(SlashCommand::Vim),
        }
    }

    /// Give the palette the values the agent offers for each dial now, each with what choosing
    /// it would do (when it applies, what it costs).
    pub(crate) fn refresh_action_choices(&mut self) {
        let options = self.agent_config().raw_options();
        let running = self.bottom_pane.is_task_running();
        let choices_of = |dial: Dial| -> Vec<Choice> {
            options
                .iter()
                .find(|option| dial.selects(option))
                .map(|option| {
                    nori_protocol::ladder(dial, option)
                        .into_iter()
                        .map(|rung| {
                            let effect = self.dial_effect(dial, &rung.value, running);
                            Choice {
                                detail: Some(describe_effect(&effect)),
                                value: rung.value,
                                label: rung.label,
                            }
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        // The roster is read without waiting: until discovery finishes it is empty.
        let roster = nori_machines::global();
        let machines = roster
            .machines()
            .map(|machine| Choice {
                value: machine.name.to_string(),
                label: machine.name.to_string(),
                detail: machine.description.clone(),
            })
            .collect();
        self.bottom_pane.set_action_choices(ActionChoices {
            models: choices_of(Dial::Model),
            efforts: choices_of(Dial::Effort),
            modes: choices_of(Dial::Mode),
            machines,
        });
    }
}

/// What a dial change does, in a few words: `next turn · prompt cache restarts`.
pub(crate) fn describe_effect(effect: &nori_protocol::DialEffect) -> String {
    use nori_protocol::AppliesAt;
    use nori_protocol::Consequence;
    let when = match effect.when {
        AppliesAt::Now => "now",
        AppliesAt::NextStep => "from the next model call",
        AppliesAt::NextTurn => "from the next turn",
        AppliesAt::AfterThisTurn => "after this turn",
    };
    let mut parts = vec![when.to_owned()];
    for cost in &effect.costs {
        parts.push(match cost {
            Consequence::PromptCacheRestarts => "prompt cache restarts".to_owned(),
            Consequence::SessionRestarts => "restarts the agent session".to_owned(),
            Consequence::ThinkingReset => "thinking starts over".to_owned(),
            Consequence::DifferentPrice { from, to } => match (from, to) {
                (Some(from), Some(to)) => format!("price {from} → {to}"),
                (None, Some(to)) => format!("price {to}"),
                (Some(_) | None, None) => "a different price".to_owned(),
            },
            Consequence::ContextShrinks { from, to } => {
                format!("context {} → {}", tokens(*from), tokens(*to))
            }
        });
    }
    parts.join(" · ")
}

fn tokens(count: i64) -> String {
    if count >= 1_000_000 {
        format!("{}M", count / 1_000_000)
    } else if count >= 1_000 {
        format!("{}k", count / 1_000)
    } else {
        count.to_string()
    }
}
