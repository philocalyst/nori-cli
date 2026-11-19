//! The overlay: a [`ChoiceSpec`] on the Helix engine, and what its actions lead to.

use std::any::Any;

use nori_tui_components::Field;
use nori_tui_components::FilterKey;
use nori_tui_components::HelixConfig;
use nori_tui_components::HelixEvent;
use nori_tui_components::HelixItem;
use nori_tui_components::HelixState;
use nori_tui_components::ItemAction;
use nori_tui_components::RowContext;
use nori_tui_components::highlight_indices;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::spec::AnswerAction;
use super::spec::Ask;
use super::spec::ChoiceAction;
use super::spec::ChoiceSpec;
use super::spec::MultiAction;
use super::spec::Next;
use super::spec::NoticeAction;
use super::spec::StepAction;
use crate::app_event_sender::AppEventSender;
use crate::bottom_pane::SelectionAction;
use crate::helix_picker::clip;
use crate::helix_picker::feed;
use crate::style::component_theme;
use crate::tui;
use crate::tui::TuiEvent;

/// What a row draws; its action stays with the overlay.
struct Row {
    index: usize,
    label: String,
    detail: Option<String>,
    keywords: Option<String>,
    section: Option<String>,
    current: bool,
    disabled: bool,
}

impl HelixItem for Row {
    type Id = usize;

    fn id(&self) -> usize {
        self.index
    }

    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>) {
        out.push(Field::new("title", &self.label, 0));
        if let Some(detail) = &self.detail {
            out.push(Field::new("detail", detail, 30));
        }
        if let Some(keywords) = &self.keywords {
            out.push(Field::new("keywords", keywords, 40));
        }
    }

    fn section(&self) -> Option<&str> {
        self.section.as_deref()
    }

    fn enabled(&self) -> bool {
        !self.disabled
    }

    fn search_identity_eq(&self, other: &Self) -> bool {
        self.label == other.label
    }

    fn rows(&self, cx: &RowContext<'_>) -> Vec<Line<'static>> {
        let width = usize::from(cx.width);
        let base = if self.disabled {
            cx.theme.faint
        } else if cx.selected {
            cx.theme.text_strong
        } else {
            Style::new()
        };
        let (mut spans, mut used) = highlight_indices(
            &self.label,
            cx.highlights.field("title"),
            base,
            cx.theme.accent_amber,
            width,
        );
        if self.current {
            spans.push(Span::styled(" ✓", cx.theme.accent_mint));
            used += 2;
        }
        if let Some(detail) = &self.detail {
            let room = width.saturating_sub(used + 2);
            if room > 4 {
                spans.push(Span::styled(
                    format!("  {}", clip(detail, room)),
                    cx.theme.faint.add_modifier(Modifier::empty()),
                ));
            }
        }
        vec![Line::from(spans)]
    }
}

const NO_FILTERS: &[FilterKey] = &[];

/// A question waiting for its answer.
struct Pending {
    then: AnswerAction,
    verb: &'static str,
    label: String,
}

pub(crate) struct ChoiceOverlay {
    state: HelixState<Row>,
    actions: Vec<ChoiceAction>,
    row_keys: Vec<Vec<(&'static str, StepAction)>>,
    keep_open: Vec<bool>,
    multi: Option<MultiAction>,
    on_back: Option<StepAction>,
    on_secondary: Option<SelectionAction>,
    on_notice: Option<NoticeAction>,
    on_dismiss: Option<SelectionAction>,
    pending: Option<Pending>,
    tx: AppEventSender,
    done: bool,
}

impl ChoiceOverlay {
    pub(crate) fn new(spec: ChoiceSpec, tx: AppEventSender) -> Self {
        let mut overlay = Self {
            state: HelixState::new(config_of(&spec)),
            actions: Vec::new(),
            row_keys: Vec::new(),
            keep_open: Vec::new(),
            multi: None,
            on_back: None,
            on_secondary: None,
            on_notice: None,
            on_dismiss: None,
            pending: None,
            tx,
            done: false,
        };
        overlay.load(spec);
        overlay
    }

    /// Show `spec` in place of whatever list there was.
    fn load(&mut self, spec: ChoiceSpec) {
        self.state =
            HelixState::new(config_of(&spec)).actions(spec.keys.iter().map(|(key, name)| {
                ItemAction {
                    name,
                    key: *key,
                    compose: None,
                    plain: true,
                }
            }));
        let mut rows = Vec::with_capacity(spec.items.len());
        let mut current = None;
        self.actions.clear();
        self.row_keys.clear();
        self.keep_open.clear();
        for (index, item) in spec.items.into_iter().enumerate() {
            if item.current && current.is_none() {
                current = Some(index);
            }
            rows.push(Row {
                index,
                label: item.label,
                detail: item.detail,
                keywords: item.keywords,
                section: item.section,
                current: item.current,
                disabled: item.disabled,
            });
            self.keep_open.push(item.keep_open);
            self.row_keys.push(item.keys);
            self.actions.push(item.action);
        }
        self.state.set_items(rows);
        if let Some(index) = spec.initial.or(current) {
            self.state.select(index);
        }
        self.multi = spec.multi;
        self.on_back = spec.on_back;
        self.on_secondary = spec.on_secondary;
        self.on_notice = spec.on_notice;
        self.on_dismiss = spec.on_dismiss;
        self.pending = None;
    }

    pub(crate) fn is_done(&self) -> bool {
        self.done
    }

    /// Tell the picker something the app learned. A picker that cares answers with a new list.
    pub(crate) fn notify(&mut self, notice: &dyn Any) {
        let Some(on_notice) = self.on_notice.clone() else {
            return;
        };
        if let Some(next) = on_notice(notice, &self.tx) {
            // A refreshed list keeps the cursor where it was.
            let cursor = self.state.selected().map(HelixItem::id);
            let shows = matches!(next, Next::Show(_));
            self.follow(next);
            if shows && let Some(cursor) = cursor {
                self.state.select(cursor);
            }
        }
    }

    fn dismiss(&mut self) {
        if let Some(on_dismiss) = &self.on_dismiss {
            on_dismiss(&self.tx);
        }
        self.done = true;
    }

    /// Carry out where an action led.
    fn follow(&mut self, next: Next) {
        match next {
            Next::Done => self.done = true,
            Next::Show(spec) => self.load(spec),
            Next::Ask(ask) => {
                if let Some(id) = self.state.selected().map(HelixItem::id) {
                    let label = if ask.label.is_empty() {
                        self.state
                            .selected()
                            .map(HelixItem::title_text)
                            .unwrap_or_default()
                    } else {
                        ask.label
                    };
                    self.state
                        .begin_compose("ask", ask.verb, label.clone(), ask.initial, id);
                    self.pending = Some(Pending {
                        then: ask.then,
                        verb: ask.verb,
                        label,
                    });
                }
            }
        }
    }

    fn apply(&mut self, event: HelixEvent<usize>) {
        match event {
            HelixEvent::Idle | HelixEvent::Redraw => {}
            HelixEvent::Cancel | HelixEvent::Interrupt => match self.on_back.clone() {
                Some(back) if matches!(event, HelixEvent::Cancel) => {
                    let next = back(&self.tx);
                    self.follow(next);
                }
                Some(_) | None => self.dismiss(),
            },
            HelixEvent::Secondary => {
                if let Some(action) = &self.on_secondary {
                    action(&self.tx);
                }
            }
            HelixEvent::Submit(index) => match self.actions.get(index) {
                Some(ChoiceAction::Run(action)) => {
                    action(&self.tx);
                    self.done = !self.keep_open.get(index).copied().unwrap_or(false);
                }
                Some(ChoiceAction::Step(step)) => {
                    let next = step(&self.tx);
                    self.follow(next);
                }
                // A row that asks first asks the way an action's question does, so its own label
                // and starting text are used and the answer reaches it.
                Some(ChoiceAction::Ask(ask)) => {
                    let next = Next::Ask(Ask {
                        verb: ask.verb,
                        label: ask.label.clone(),
                        initial: ask.initial.clone(),
                        then: ask.then.clone(),
                    });
                    self.follow(next);
                }
                None => {}
            },
            HelixEvent::SubmitMany(indexes) => {
                if let Some(multi) = &self.multi {
                    multi(&indexes, &self.tx);
                    self.done = true;
                }
            }
            HelixEvent::Action(name, index) => {
                let step = self
                    .row_keys
                    .get(index)
                    .and_then(|keys| keys.iter().find(|(key, _)| *key == name))
                    .map(|(_, step)| step.clone());
                if let Some(step) = step {
                    let next = step(&self.tx);
                    self.follow(next);
                }
            }
            HelixEvent::Composed(_, _, text) => {
                let Some(pending) = self.pending.take() else {
                    return;
                };
                match (pending.then)(&text, &self.tx) {
                    Ok(next) => self.follow(next),
                    Err(reason) => {
                        // The question stays, with what was typed.
                        self.state.set_status(Some(reason));
                        if let Some(id) = self.state.selected().map(HelixItem::id) {
                            self.state.begin_compose(
                                "ask",
                                pending.verb,
                                pending.label.clone(),
                                text,
                                id,
                            );
                        }
                        self.pending = Some(pending);
                    }
                }
            }
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> std::io::Result<()> {
        match event {
            TuiEvent::Draw => {
                let theme = component_theme();
                let height = self.height().max(6);
                // The overlay's frame starts wherever the stage's region does, which is below
                // the shell's own rows when Nori was started mid-screen.
                tui.draw_overlay(|frame| {
                    let frame_area = frame.area();
                    let area = Rect {
                        height: height.min(frame_area.height),
                        ..frame_area
                    };
                    self.state.render(area, frame.buffer, &theme, 0);
                })?;
            }
            TuiEvent::Key(_) | TuiEvent::Paste(_) | TuiEvent::Mouse(_) => {
                if let Some(helix) = feed(&mut self.state, &event) {
                    self.state.set_status(None);
                    let asked = self.pending.is_some();
                    self.apply(helix);
                    // A question that was cancelled (Esc) is no longer pending.
                    if asked && self.pending.is_some() && !self.state.composing() {
                        self.pending = None;
                    }
                }
                tui.frame_requester().schedule_frame();
            }
        }
        Ok(())
    }

    /// As tall as the choices need, not the whole screen: rows, section headings and chrome.
    fn height(&self) -> u16 {
        let mut headings = 0;
        let mut previous: Option<&str> = None;
        for row in self.state.items() {
            let section = row.section.as_deref();
            if section.is_some() && section != previous {
                headings += 1;
            }
            previous = section;
        }
        u16::try_from(self.state.items().len() + headings + 4).unwrap_or(u16::MAX)
    }
}

fn config_of(spec: &ChoiceSpec) -> HelixConfig {
    HelixConfig {
        title: spec.title.clone(),
        placeholder: if spec.searchable {
            "search".to_owned()
        } else {
            spec.hint.clone()
        },
        empty_text: "Nothing to choose from.".to_owned(),
        no_match_text: "No match.".to_owned(),
        filter_keys: NO_FILTERS,
        preview: false,
        multi: spec.multi.is_some(),
        searchable: spec.searchable,
        on_demand: false,
        numbered: false,
        subtitle: spec.subtitle.clone(),
    }
}

/// The loop count picker, and a sectioned model list with an unavailable row, as plain text for
/// the storybook (feature `storybook`).
#[cfg(feature = "storybook")]
pub fn choice_specimen(width: u16) -> String {
    use ratatui::buffer::Buffer;

    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let tx = AppEventSender::new(tx);
    let row = |name: &str, agent: &str, also: Option<&str>, current: bool, available: bool| {
        crate::bottom_pane::SelectionItem {
            name: name.to_owned(),
            search_value: Some(format!("{name} {agent}")),
            description: also.map(|also| format!("also on {also}")).or_else(|| {
                (!available).then(|| "unavailable: quota exhausted until 18:00".to_owned())
            }),
            is_current: current,
            actions: if available {
                vec![Box::new(|_| {})]
            } else {
                Vec::new()
            },
            dismiss_on_select: available,
            ..Default::default()
        }
    };
    let header = |name: &str| crate::bottom_pane::SelectionItem {
        name: name.to_owned(),
        is_header: true,
        ..Default::default()
    };
    let models = ChoiceSpec::from(crate::bottom_pane::SelectionViewParams {
        title: Some("Select model".to_owned()),
        items: vec![
            header("Codex"),
            row("GPT-6.2 Sol", "Codex", Some("OpenCode"), true, true),
            row("GPT-6.1 Terra", "Codex", None, false, true),
            header("Claude Code"),
            row("Opus", "Claude Code", None, false, true),
            header("Unavailable"),
            row("Gemini 3.8 Flash", "Antigravity", None, false, false),
        ],
        is_searchable: true,
        ..Default::default()
    });
    [
        crate::nori::loop_count_picker::loop_count_choices(Some(7)),
        models,
        crate::nori::mcp_server_picker::mcp_specimen(),
    ]
    .into_iter()
    .map(|spec| {
        let mut overlay = ChoiceOverlay::new(spec, tx.clone());
        let area = Rect::new(0, 3, width, 12);
        let mut buffer = Buffer::empty(area);
        overlay
            .state
            .render(area, &mut buffer, &component_theme(), 0);
        (area.y..area.bottom())
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
    .collect::<Vec<_>>()
    .join("\n")
}
