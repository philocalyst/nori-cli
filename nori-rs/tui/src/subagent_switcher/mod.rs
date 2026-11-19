//! The actor panel: the live tree of the main session and every subagent as one Helix picker.
//! Search across what each said, preview the highlighted actor's conversation as it runs, and
//! act on it: Enter focuses it in the chat; `Alt+M` messages it, `Alt+S` steers it, `Alt+X`
//! stops its turn.
//!
//! The panel takes snapshots ([`Roster`], [`Body`]) and never reaches into the chat widget. The
//! app is the go-between: before each frame it asks what the panel lacks
//! ([`SubagentOverlay::body_query`]), fetches that, and hands it back with a fresh roster
//! ([`SubagentOverlay::update`]). A frame in which nothing changed asks for nothing.

mod item;
mod preview;
mod roster;
#[cfg(feature = "storybook")]
pub mod specimen;
mod tail;
mod tree;

use std::time::Instant;

use crossterm::event::KeyEvent;
use nori_config::HotkeyBinding;
use nori_tui_components::FilterKey;
use nori_tui_components::HelixConfig;
use nori_tui_components::HelixEvent;
use nori_tui_components::HelixState;
use nori_tui_components::ItemAction;
use nori_tui_components::PreviewRequest;
use ratatui::layout::Rect;

use item::ActorItem;
pub(crate) use roster::Body;
pub(crate) use roster::BodyQuery;
pub(crate) use roster::Entry;
pub(crate) use roster::Parent;
pub(crate) use roster::Roster;
pub(crate) use roster::Status;
pub(crate) use roster::SubagentKey;
pub(crate) use roster::Target;
pub(crate) use tail::Tail;
pub(crate) use tail::cell_lines;
use tree::FirstSeen;

use crate::helix_picker::feed;
use crate::nori::hotkey_match::matches_binding;
use crate::style::component_theme;
use crate::tui;
use crate::tui::TuiEvent;

const FILTERS: &[FilterKey] = &[
    FilterKey::new("agent", Some('@')),
    FilterKey::new("model", None),
    FilterKey::new("status", Some('%')),
    FilterKey::new("parent", None),
    FilterKey::new("detail", None),
];

const ACTIONS: [ItemAction; 3] = [
    ItemAction {
        name: "message",
        key: 'm',
        compose: Some("message"),
        plain: false,
    },
    ItemAction {
        name: "steer",
        key: 's',
        compose: Some("steer"),
        plain: false,
    },
    ItemAction {
        name: "cancel",
        key: 'x',
        compose: None,
        plain: false,
    },
];

/// What the user asked for that the panel cannot do itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Transition {
    /// Leave for the normal chat view.
    Close,
    /// Focus the main conversation in the chat.
    FocusMain,
    /// Focus a subagent's conversation in the chat.
    Focus(SubagentKey),
    /// Send a subagent a message; `steer` interrupts its turn first.
    Message {
        target: SubagentKey,
        text: String,
        steer: bool,
    },
    /// Stop a subagent's running turn.
    Cancel(SubagentKey),
}

pub(crate) struct SubagentOverlay {
    state: HelixState<ActorItem>,
    toggle: HotkeyBinding,
    transition: Option<Transition>,
    first_seen: FirstSeen,
    /// The preview the panel last asked for, answered by [`Self::set_body`].
    asked: Option<PreviewRequest<Target>>,
    tick: i64,
    /// The area the panel was last drawn in.
    area: Rect,
}

impl SubagentOverlay {
    pub(crate) fn switcher(roster: &Roster, toggle: HotkeyBinding) -> Self {
        let state = HelixState::new(HelixConfig {
            title: "Actors".to_owned(),
            placeholder: "search actors".to_owned(),
            empty_text: "No child conversations yet.".to_owned(),
            no_match_text: "No actor matches. Try fewer words, or remove a filter.".to_owned(),
            filter_keys: FILTERS,
            preview: true,
            multi: false,
            searchable: true,
            on_demand: false,
            numbered: false,
            subtitle: None,
        })
        .actions(ACTIONS);
        let mut overlay = Self {
            state,
            toggle,
            transition: None,
            first_seen: FirstSeen::new(),
            asked: None,
            tick: roster.tick,
            area: Rect::default(),
        };
        overlay.update(roster, Rect::default());
        overlay
    }

    pub(crate) fn take_transition(&mut self) -> Option<Transition> {
        self.transition.take()
    }

    pub(crate) fn is_done(&self) -> bool {
        self.transition == Some(Transition::Close)
    }

    /// The conversation tail the preview needs and does not have yet.
    pub(crate) fn body_query(&mut self, area: Rect, now: Instant) -> Option<BodyQuery> {
        // The area the panel was drawn in, once it has been; before that, the one given.
        let area = if self.area.is_empty() {
            area
        } else {
            self.area
        };
        let request = self.state.preview_request(area, now)?;
        let query = BodyQuery {
            target: request.id,
            width: request.width,
            rows: request.rows,
        };
        self.asked = Some(request);
        Some(query)
    }

    /// Take a fresh roster. Searches again only when something the search looks at changed.
    pub(crate) fn update(&mut self, roster: &Roster, _area: Rect) {
        self.tick = roster.tick;
        let selected = self
            .state
            .selected()
            .map(|item| roster.resolve(item.entry.target));
        self.state
            .set_items(tree::arrange(roster, &mut self.first_seen));
        if let Some(target) = selected {
            self.state.select(target);
        }
    }

    pub(crate) fn set_body(&mut self, _roster: &Roster, body: Body, _area: Rect) {
        let Some(request) = self
            .asked
            .take()
            .filter(|asked| asked.id == body.query.target)
        else {
            return;
        };
        let Some(item) = self
            .state
            .items()
            .iter()
            .find(|item| item.entry.target == request.id)
        else {
            return;
        };
        let built = preview::build(&request, item, &body, self.tick, &component_theme());
        self.state.set_preview(built);
    }

    fn apply(&mut self, event: HelixEvent<Target>) {
        self.transition = match event {
            HelixEvent::Idle | HelixEvent::Redraw => return,
            HelixEvent::Cancel | HelixEvent::Interrupt => Some(Transition::Close),
            HelixEvent::Submit(Target::Main) => Some(Transition::FocusMain),
            HelixEvent::Submit(Target::Subagent(key)) => Some(Transition::Focus(key)),
            HelixEvent::Action("cancel", Target::Subagent(key)) => Some(Transition::Cancel(key)),
            HelixEvent::Composed(action, Target::Subagent(target), text) => {
                Some(Transition::Message {
                    target,
                    text,
                    steer: action == "steer",
                })
            }
            // The root takes no messages and no cancel from here: its composer does.
            HelixEvent::Action(_, Target::Main) | HelixEvent::Composed(_, Target::Main, _) => {
                Some(Transition::FocusMain)
            }
            HelixEvent::Action(..) | HelixEvent::SubmitMany(_) | HelixEvent::Secondary => return,
        };
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) {
        if matches_binding(&self.toggle, &key) {
            self.transition = Some(Transition::Close);
            return;
        }
        let event = self.state.on_key(key);
        self.apply(event);
    }

    pub(crate) fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> std::io::Result<()> {
        match event {
            TuiEvent::Key(key) => self.on_key(key),
            TuiEvent::Paste(_) | TuiEvent::Mouse(_) => {
                if let Some(helix) = feed(&mut self.state, &event) {
                    self.apply(helix);
                }
            }
            TuiEvent::Draw => {
                let theme = component_theme();
                tui.draw_overlay(|frame| {
                    self.area = frame.area();
                    self.state
                        .render(frame.area(), frame.buffer, &theme, self.tick);
                })?;
                return Ok(());
            }
        }
        tui.frame_requester().schedule_frame();
        Ok(())
    }
}
