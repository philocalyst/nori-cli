//! The action palette in the composer, and the directives it puts into the draft.
//!
//! The palette is a popup with its own query line: opening it, searching and choosing never
//! change the draft. An action that applies on send, chosen while there is a draft, becomes a
//! directive: a chip in the draft (`⚙ model: opus`) that is taken out of the text when the
//! prompt is sent and applied just before it.

use super::*;
use crate::bottom_pane::palette::ActionChoices;
use crate::bottom_pane::palette::Palette;
use crate::bottom_pane::palette::PaletteOutcome;
use nori_actions::Invocation;
use nori_config::KeyAction;
use nori_config::KeyMode;

impl ChatComposer {
    pub(crate) fn set_action_choices(&mut self, choices: ActionChoices) {
        self.action_choices = choices;
    }

    /// Whether `key` is the palette's key (`composer.palette`, Alt+X by default).
    pub(super) fn is_palette_key(&self, key: &KeyEvent) -> bool {
        key.kind == KeyEventKind::Press
            && self.keymap.find(
                KeyMode::Composer,
                self.textarea.hotkey_config(),
                |binding| matches_binding(binding, key),
            ) == Some(KeyAction::Palette)
    }

    /// Open the palette over the composer. The draft stays as it is.
    pub(crate) fn open_palette(&mut self) {
        if !self.input_enabled {
            return;
        }
        let situation = crate::actions::situation(
            self.is_task_running,
            self.child_context,
            self.cloud_session.is_some(),
        );
        self.active_popup = ActivePopup::Palette(Box::new(Palette::new(
            self.actions.clone(),
            self.action_choices.clone(),
            situation,
            !self.is_empty(),
        )));
    }

    pub(super) fn handle_key_event_with_palette(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        let ActivePopup::Palette(palette) = &mut self.active_popup else {
            return (InputResult::None, false);
        };
        match palette.handle_key(key_event) {
            PaletteOutcome::Open => (InputResult::None, true),
            PaletteOutcome::Closed => {
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            PaletteOutcome::Run(invocation) => {
                self.active_popup = ActivePopup::None;
                // An agent's command or a saved prompt is a prompt of its own: it goes to the
                // front of the draft, which becomes its argument.
                if let Some(name) = invocation
                    .id
                    .as_str()
                    .strip_prefix("agent.")
                    .or_else(|| invocation.id.as_str().strip_prefix("prompt."))
                {
                    let name = if invocation.id.as_str().starts_with("prompt.") {
                        format!("{PROMPTS_CMD_PREFIX}:{name}")
                    } else {
                        name.to_owned()
                    };
                    self.prefix_draft_with_command(&name, invocation.arg.as_deref());
                    return (InputResult::None, true);
                }
                (InputResult::Action(invocation), true)
            }
            PaletteOutcome::Attach(invocation, label) => {
                self.active_popup = ActivePopup::None;
                self.attach_directive(invocation, &label);
                (InputResult::None, true)
            }
        }
    }

    /// Every action the palette offers: Nori's, the agent's commands and the saved prompts.
    pub(super) fn rebuild_actions(&mut self) {
        use nori_actions::Action;
        use nori_actions::Category;
        use nori_actions::Param;
        use nori_actions::Source;
        let mut registry = crate::actions::builtin_registry(self.textarea.hotkey_config());
        for command in &self.agent_commands {
            let name = if self.agent_command_prefix.is_empty() {
                command.name.clone()
            } else {
                format!("{}:{}", self.agent_command_prefix, command.name)
            };
            registry.register(
                Action::new(
                    &format!("agent.{name}"),
                    &format!("/{name}"),
                    &command.description,
                    Category::Agent,
                )
                .named(&[&name])
                .param(Param::Text {
                    placeholder: command.input_hint.clone().unwrap_or_default(),
                    required: false,
                })
                .source(Source::Agent),
            );
        }
        for prompt in &self.custom_prompts {
            let name = format!("{PROMPTS_CMD_PREFIX}:{}", prompt.name);
            registry.register(
                Action::new(
                    &format!("prompt.{}", prompt.name),
                    &format!("/{name}"),
                    prompt.description.as_deref().unwrap_or("send saved prompt"),
                    Category::Conversation,
                )
                .named(&[&name])
                .source(Source::Prompt),
            );
        }
        for action in self.plugin_actions.iter() {
            registry.register(action.clone());
        }
        self.actions = std::sync::Arc::new(registry);
    }

    /// The plugins' actions changed (a module loaded): the palette offers the new set.
    pub(crate) fn set_plugin_actions(&mut self, actions: std::sync::Arc<[nori_actions::Action]>) {
        if std::sync::Arc::ptr_eq(&self.plugin_actions, &actions) {
            return;
        }
        self.plugin_actions = actions;
        self.rebuild_actions();
    }

    /// Who defined the action `id`, when the palette knows it.
    pub(crate) fn action_source(
        &self,
        id: &nori_actions::ActionId,
    ) -> Option<nori_actions::Source> {
        self.actions.get(id).map(|action| action.source.clone())
    }

    /// Put `/name value ` at the front of the draft, the cursor at its end.
    fn prefix_draft_with_command(&mut self, name: &str, value: Option<&str>) {
        let mut prefix = format!("/{name} ");
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            prefix.push_str(value);
            prefix.push(' ');
        }
        self.textarea.replace_range(0..0, &prefix);
        self.textarea.set_cursor(prefix.len());
        self.textarea.enter_insert();
        self.textarea.set_cursor(prefix.len());
    }

    /// Put `invocation` into the draft at the cursor, as a chip applied when the draft is sent.
    pub(crate) fn attach_directive(&mut self, invocation: Invocation, value_label: &str) {
        let label = crate::actions::directive_label(&self.actions, &invocation, value_label);
        // A directive of the same action replaces an earlier one: the newest choice stands.
        let stale: Vec<ChipId> = self
            .directives
            .iter()
            .filter(|(_, existing)| existing.id == invocation.id)
            .map(|(chip, _)| *chip)
            .collect();
        for chip in stale {
            let text = self.textarea.text().to_owned();
            if let Some(at) = text.find(chip.as_char()) {
                self.textarea
                    .replace_range(at..at + chip.as_char().len_utf8(), "");
            }
            self.directives.retain(|(existing, _)| *existing != chip);
        }
        let needs_space = self
            .textarea
            .text()
            .get(..self.textarea.cursor())
            .and_then(|before| before.chars().next_back())
            .is_some_and(|ch| !ch.is_whitespace());
        if needs_space {
            self.textarea.insert_at_cursor(" ");
        }
        let chip = self.textarea.insert_chip(&label, ChipTone::Directive);
        let space_follows = self
            .textarea
            .text()
            .get(self.textarea.cursor()..)
            .and_then(|after| after.chars().next())
            .is_some_and(char::is_whitespace);
        if !space_follows {
            self.textarea.insert_at_cursor(" ");
        }
        self.directives.push((chip, invocation));
    }

    /// Take the directives whose chips are in `text` (the draft being sent) out of the draft's
    /// list, to be applied when it is sent.
    pub(super) fn collect_directives(&mut self, text: &str) {
        let (sent, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.directives)
            .into_iter()
            .partition(|(chip, _)| text.contains(chip.as_char()));
        self.directives = kept;
        self.sent_directives = sent;
    }

    /// The directives of the prompt just sent, to apply before it goes to the agent.
    pub(crate) fn take_directives(&mut self) -> Vec<Invocation> {
        std::mem::take(&mut self.sent_directives)
            .into_iter()
            .map(|(_, invocation)| invocation)
            .collect()
    }

    /// Whether `id` is a directive chip (of this draft, or of the prompt being sent).
    pub(super) fn is_directive(&self, id: ChipId) -> bool {
        self.directives
            .iter()
            .chain(&self.sent_directives)
            .any(|(chip, _)| *chip == id)
    }
}
