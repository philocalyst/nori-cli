//! The action palette: every action, searchable, from anywhere in a draft.
//!
//! It opens over the composer (`Ctrl+K`, Helix's `:`, `/` on an empty draft) with its own query
//! line, so the draft is never touched. Choosing an action that takes a value lists the values;
//! with a draft open, an action that applies on send ([`nori_actions::Effect::OnSend`]) is put
//! into the draft as a directive chip instead of running now.

use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use nori_actions::Action;
use nori_actions::Choice;
use nori_actions::ChoiceSource;
use nori_actions::Effect;
use nori_actions::Invocation;
use nori_actions::Param;
use nori_actions::Registry;
use nori_actions::Situation;
use nori_editor::Editor;
use nori_editor::EditorStyle;
use nori_editor::KeymapKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;

use super::popup_consts::MAX_COMMAND_POPUP_ROWS;
use super::scroll_state::ScrollState;
use super::selection_popup_common::GenericDisplayRow;
use super::selection_popup_common::measure_rows_height;
use super::selection_popup_common::render_rows;

/// The values of the dials the agent offers now, for the palette's second step.
#[derive(Clone, Debug, Default)]
pub(crate) struct ActionChoices {
    pub(crate) models: Vec<Choice>,
    pub(crate) efforts: Vec<Choice>,
    pub(crate) modes: Vec<Choice>,
    /// The user's other computers, from the machine roster.
    pub(crate) machines: Vec<Choice>,
}

impl ActionChoices {
    /// The values `source` offers, when the palette can list them itself.
    fn of(&self, source: &ChoiceSource) -> Option<Vec<Choice>> {
        match source {
            ChoiceSource::Models => Some(self.models.clone()),
            ChoiceSource::Efforts => Some(self.efforts.clone()),
            ChoiceSource::Modes => Some(self.modes.clone()),
            ChoiceSource::Machines => Some(self.machines.clone()),
            ChoiceSource::Fixed(choices) => Some(choices.clone()),
            // These have pickers of their own: the action opens them.
            ChoiceSource::Agents
            | ChoiceSource::Skills
            | ChoiceSource::Files
            | ChoiceSource::Sessions => None,
        }
        .filter(|choices| !choices.is_empty())
    }
}

/// What a key did to the palette.
#[derive(Debug, PartialEq)]
pub(crate) enum PaletteOutcome {
    /// Still open.
    Open,
    Closed,
    /// Run this now.
    Run(Invocation),
    /// Put this into the draft as a directive, shown as the value's label.
    Attach(Invocation, String),
}

enum Stage {
    Actions,
    /// Choosing the value of an action.
    Values {
        action: Box<Action>,
        choices: Vec<Choice>,
    },
}

pub(crate) struct Palette {
    registry: Arc<Registry>,
    choices: ActionChoices,
    situation: Situation,
    /// Opened with a draft in the composer: on-send actions become directives.
    over_draft: bool,
    query: Editor,
    stage: Stage,
    state: ScrollState,
}

impl Palette {
    pub(crate) fn new(
        registry: Arc<Registry>,
        choices: ActionChoices,
        situation: Situation,
        over_draft: bool,
    ) -> Self {
        let mut palette = Palette {
            registry,
            choices,
            situation,
            over_draft,
            query: Editor::new(KeymapKind::Standard),
            stage: Stage::Actions,
            state: ScrollState::new(),
        };
        palette.state.clamp_selection(palette.len());
        palette
    }

    /// The query as typed, and the part after the first word (a value given inline:
    /// `model opus`).
    fn query(&self) -> (&str, Option<&str>) {
        let text = self.query.text().trim_start();
        let text = text.strip_prefix('/').unwrap_or(text);
        match text.split_once(char::is_whitespace) {
            Some((word, rest)) if !rest.trim().is_empty() => (word, Some(rest.trim())),
            Some((word, _)) => (word, None),
            None => (text, None),
        }
    }

    fn hits(&self) -> Vec<nori_actions::Hit<'_>> {
        let (word, _) = self.query();
        self.registry.search(word, &self.situation)
    }

    fn values(&self) -> Vec<&Choice> {
        let Stage::Values { choices, .. } = &self.stage else {
            return Vec::new();
        };
        let wanted = self.query.text().trim().to_lowercase();
        choices
            .iter()
            .filter(|choice| {
                wanted.is_empty()
                    || choice.label.to_lowercase().contains(&wanted)
                    || choice.value.to_lowercase().contains(&wanted)
            })
            .collect()
    }

    fn len(&self) -> usize {
        match self.stage {
            Stage::Actions => self.hits().len(),
            Stage::Values { .. } => self.values().len(),
        }
    }

    pub(crate) fn handle_key(&mut self, key: KeyEvent) -> PaletteOutcome {
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => match self.stage {
                Stage::Values { .. } => {
                    self.stage = Stage::Actions;
                    self.query.set_text("");
                    self.state.reset();
                    self.state.clamp_selection(self.len());
                    PaletteOutcome::Open
                }
                Stage::Actions => PaletteOutcome::Closed,
            },
            (KeyCode::Up, _) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                let len = self.len();
                self.state.move_up_wrap(len);
                self.state
                    .ensure_visible(len, MAX_COMMAND_POPUP_ROWS.min(len));
                PaletteOutcome::Open
            }
            (KeyCode::Down, _) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                let len = self.len();
                self.state.move_down_wrap(len);
                self.state
                    .ensure_visible(len, MAX_COMMAND_POPUP_ROWS.min(len));
                PaletteOutcome::Open
            }
            (KeyCode::Enter | KeyCode::Tab, _) => self.choose(),
            _ => {
                let before = self.query.revision();
                self.query.handle_key(key);
                if self.query.revision() != before {
                    self.state.reset();
                    self.state.clamp_selection(self.len());
                }
                PaletteOutcome::Open
            }
        }
    }

    fn choose(&mut self) -> PaletteOutcome {
        let selected = self.state.selected_idx.unwrap_or(0);
        match &self.stage {
            Stage::Actions => {
                let (_, inline) = self.query();
                let inline = inline.map(str::to_owned);
                let Some(hit) = self.hits().into_iter().nth(selected) else {
                    return PaletteOutcome::Open;
                };
                if hit.unavailable.is_some() {
                    return PaletteOutcome::Open;
                }
                let action = hit.action.clone();
                match (&action.param, inline) {
                    (Param::None, _) => self.finish(&action, None),
                    (Param::Choice(_) | Param::Text { .. }, Some(value)) => {
                        self.finish(&action, Some(value))
                    }
                    (Param::Choice(source), None) => match self.choices.of(source) {
                        Some(choices) => {
                            self.stage = Stage::Values {
                                action: Box::new(action),
                                choices,
                            };
                            self.query.set_text("");
                            self.state.reset();
                            self.state.clamp_selection(self.len());
                            PaletteOutcome::Open
                        }
                        None => self.finish(&action, None),
                    },
                    (Param::Text { .. }, None) => self.finish(&action, None),
                }
            }
            Stage::Values { action, .. } => {
                let action = action.as_ref().clone();
                let Some((value, label)) = self.values().get(selected).map(|choice| match &choice
                    .detail
                {
                    Some(effect) => (choice.value.clone(), format!("{} · {effect}", choice.label)),
                    None => (choice.value.clone(), choice.label.clone()),
                }) else {
                    return PaletteOutcome::Open;
                };
                match self.finish(&action, Some(value)) {
                    PaletteOutcome::Attach(invocation, _) => {
                        PaletteOutcome::Attach(invocation, label)
                    }
                    other => other,
                }
            }
        }
    }

    fn finish(&self, action: &Action, value: Option<String>) -> PaletteOutcome {
        let directive = self.over_draft && action.effect == Effect::OnSend && value.is_some();
        let label = value.clone().unwrap_or_default();
        let invocation = Invocation::new(&action.id, value);
        if directive {
            PaletteOutcome::Attach(invocation, label)
        } else {
            PaletteOutcome::Run(invocation)
        }
    }

    fn rows(&self) -> Vec<GenericDisplayRow> {
        match &self.stage {
            Stage::Actions => self
                .hits()
                .into_iter()
                .map(|hit| {
                    let action = hit.action;
                    let mut description = action.summary.clone();
                    if let Some(keys) = action.keys.first().filter(|keys| !keys.is_empty()) {
                        description.push_str(&format!("  ({keys})"));
                    }
                    let styled = hit
                        .unavailable
                        .map(|reason| Line::from(format!("{} — {reason}", action.summary)).dim());
                    GenericDisplayRow {
                        name: match action.name() {
                            Some(name) => format!("/{name}"),
                            None => action.title.clone(),
                        },
                        display_shortcut: None,
                        match_indices: None,
                        description: Some(description),
                        styled_description: styled,
                        disabled: hit.unavailable.is_some(),
                        is_header: false,
                        two_line: false,
                    }
                })
                .collect(),
            Stage::Values { .. } => self
                .values()
                .into_iter()
                .map(|choice| GenericDisplayRow {
                    name: choice.label.clone(),
                    display_shortcut: None,
                    match_indices: None,
                    description: choice.detail.clone(),
                    styled_description: None,
                    disabled: false,
                    is_header: false,
                    two_line: false,
                })
                .collect(),
        }
    }

    pub(crate) fn calculate_required_height(&self, width: u16) -> u16 {
        1 + measure_rows_height(
            &self.rows(),
            &self.state,
            MAX_COMMAND_POPUP_ROWS,
            width.saturating_sub(2),
        )
    }

    pub(crate) fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let prompt = match &self.stage {
            Stage::Actions if self.over_draft => "action (draft kept) › ".to_owned(),
            Stage::Actions => "action › ".to_owned(),
            Stage::Values { action, .. } => format!("{} › ", action.title),
        };
        let prompt_width = u16::try_from(unicode_width::UnicodeWidthStr::width(prompt.as_str()))
            .unwrap_or(0)
            .min(area.width);
        Line::from(Span::from(prompt).cyan()).render(Rect { height: 1, ..area }, buf);
        let query_area = Rect {
            x: area.x + prompt_width,
            width: area.width - prompt_width,
            height: 1,
            ..area
        };
        self.query
            .render(query_area, buf, &EditorStyle::default(), &[]);
        let list = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            x: area.x + 2,
            width: area.width.saturating_sub(2),
        };
        render_rows(
            list,
            buf,
            &self.rows(),
            &self.state,
            MAX_COMMAND_POPUP_ROWS,
            "no matching action",
        );
    }

    /// Where the terminal cursor goes: in the query line.
    pub(crate) fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        let prompt = match &self.stage {
            Stage::Actions if self.over_draft => "action (draft kept) › ".to_owned(),
            Stage::Actions => "action › ".to_owned(),
            Stage::Values { action, .. } => format!("{} › ", action.title),
        };
        let prompt_width =
            u16::try_from(unicode_width::UnicodeWidthStr::width(prompt.as_str())).unwrap_or(0);
        let query_area = Rect {
            x: area.x + prompt_width.min(area.width),
            width: area.width.saturating_sub(prompt_width),
            height: 1,
            ..area
        };
        self.query
            .cursor_position(query_area)
            .map(|position| (position.x, position.y))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn palette(over_draft: bool) -> Palette {
        let registry = Arc::new(crate::actions::builtin_registry(
            &nori_config::HotkeyConfig::default(),
        ));
        let choices = ActionChoices {
            models: vec![
                Choice {
                    value: "opus".into(),
                    label: "Opus".into(),
                    detail: None,
                },
                Choice {
                    value: "sonnet".into(),
                    label: "Sonnet".into(),
                    detail: None,
                },
            ],
            ..ActionChoices::default()
        };
        Palette::new(registry, choices, Situation::default(), over_draft)
    }

    fn typed(palette: &mut Palette, text: &str) -> PaletteOutcome {
        let mut outcome = PaletteOutcome::Open;
        for ch in text.chars() {
            outcome = palette.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        outcome
    }

    fn enter(palette: &mut Palette) -> PaletteOutcome {
        palette.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
    }

    #[test]
    fn a_dial_lists_its_values_then_attaches_over_a_draft() {
        let mut palette = palette(true);
        typed(&mut palette, "model");
        assert_eq!(enter(&mut palette), PaletteOutcome::Open);
        typed(&mut palette, "son");
        assert_eq!(
            enter(&mut palette),
            PaletteOutcome::Attach(
                Invocation::new(
                    &nori_actions::ActionId::new("model.set"),
                    Some("sonnet".into())
                ),
                "Sonnet".into()
            )
        );
    }

    #[test]
    fn on_an_empty_draft_it_runs_and_takes_an_inline_value() {
        let mut palette = palette(false);
        typed(&mut palette, "model opus");
        assert_eq!(
            enter(&mut palette),
            PaletteOutcome::Run(Invocation::new(
                &nori_actions::ActionId::new("model.set"),
                Some("opus".into())
            ))
        );
    }

    #[test]
    fn escape_steps_back_then_closes() {
        let mut palette = palette(false);
        typed(&mut palette, "model");
        enter(&mut palette);
        let escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(palette.handle_key(escape), PaletteOutcome::Open);
        assert_eq!(palette.handle_key(escape), PaletteOutcome::Closed);
    }
}
