//! Walking back through the prompts of the conversation from the composer, like shell history.
//!
//! `↑` with the caret on the composer's first row (or an empty composer) loads the previous
//! prompt of the main conversation into the composer; the draft that was there is stashed and
//! comes back when the walk ends. Further `↑`/`↓` step between prompts, keeping what was edited
//! in each for as long as the walk lasts. Past the oldest prompt `↑` goes on into the history of
//! earlier sessions (plain recall: Enter sends it as a new prompt). `↓` past the newest prompt,
//! `Esc` (once the editor has no use for it) and `Ctrl+C` end the walk. `↓` on an empty composer
//! brings back the draft `Ctrl+C` last cleared.
//!
//! While a prompt of the conversation is loaded, `Enter` rewinds the conversation to before it
//! and sends the edited text (restoring the files the rewound turns changed), `Alt+Enter` does
//! the same keeping the files. The app hears every step ([`AppEvent::FocusChanged`]) to show
//! which prompt is walked to and what a rewind would discard.

use std::collections::HashMap;

use super::*;
use crate::app_event::RewindFiles;
use crate::app_event::RewindSelection;
use crate::app_event::SessionGeneration;
use crate::bottom_pane::chat_composer_history::Recall;
use unicode_width::UnicodeWidthStr;

/// A prompt of the main conversation, as the composer recalls it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecalledPrompt {
    /// Its index in the transcript.
    pub(crate) cell_index: usize,
    pub(crate) text: String,
}

/// Identifies one state of the transcript's prompts, so the app only rebuilds the list when the
/// transcript changed: its length and the address of its last cell.
pub(crate) type PromptsFingerprint = (usize, usize);

/// The prompts of the main conversation, oldest first, as of `fingerprint`.
#[derive(Default)]
pub(super) struct ConversationPrompts {
    generation: SessionGeneration,
    fingerprint: PromptsFingerprint,
    prompts: Vec<RecalledPrompt>,
}

/// Where a walk is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Zone {
    /// On a prompt of the conversation: its place in [`ConversationPrompts::prompts`].
    Conversation(usize),
    /// Past the oldest prompt, in the history of earlier sessions.
    History,
}

pub(super) struct PromptWalk {
    /// The draft the walk started from.
    stash: ComposerDraft,
    zone: Zone,
    /// What was edited in each prompt walked over, by place.
    edits: HashMap<usize, String>,
}

/// What the composer's border says while a walk is on.
pub(crate) enum WalkBadge {
    Turn { turn: usize, total: usize },
    History,
}

impl WalkBadge {
    /// The line that says where the walk is and what the keys do from here, `width` columns at
    /// most: keys are left out whole, least needed first, and none is ever cut in two. Leaving is
    /// always said.
    pub(crate) fn hint(&self, width: u16) -> Line<'static> {
        let theme = crate::style::component_theme();
        let (place, short_place, keys): (String, String, &[&str]) = match self {
            WalkBadge::Turn { turn, total } => (
                format!("↶ turn {turn} of {total}"),
                format!("↶ {turn}/{total}"),
                &["⏎ rewind here", "esc back", "⌥⏎ keep files", "^F fork"],
            ),
            WalkBadge::History => (
                "↶ earlier session".to_owned(),
                "↶ earlier".to_owned(),
                &["⏎ send as new", "esc back"],
            ),
        };
        let width = usize::from(width);
        let fits = |place: &str, count: usize| {
            place.width()
                + keys[..count]
                    .iter()
                    .map(|key| key.width() + 3)
                    .sum::<usize>()
                <= width
        };
        // The fullest line that fits: every key with the long place, then fewer keys, then the
        // short place; `⏎` and `esc back` are the last to go.
        let (place, count) = (0..=keys.len())
            .rev()
            .flat_map(|count| [(place.as_str(), count), (short_place.as_str(), count)])
            .find(|(place, count)| fits(place, *count))
            .unwrap_or((short_place.as_str(), 0));
        // Shown in the order they are read, whatever order they are dropped in.
        let order = [
            "⏎ rewind here",
            "⏎ send as new",
            "⌥⏎ keep files",
            "^F fork",
            "esc back",
        ];
        let mut shown: Vec<&str> = keys[..count].to_vec();
        shown.sort_by_key(|key| order.iter().position(|known| known == key));
        let mut spans = vec![Span::styled(
            place.to_owned(),
            theme
                .accent_amber
                .add_modifier(ratatui::style::Modifier::BOLD),
        )];
        for key in shown {
            spans.push(Span::from(format!(" · {key}")).dim());
        }
        Line::from(spans)
    }
}

impl ChatComposer {
    /// Whether the prompts last given are those of `generation` and `fingerprint`.
    pub(crate) fn conversation_prompts_current(
        &self,
        generation: SessionGeneration,
        fingerprint: PromptsFingerprint,
    ) -> bool {
        self.conversation_prompts.generation == generation
            && self.conversation_prompts.fingerprint == fingerprint
    }

    /// The prompts of the main conversation changed. A walk on a prompt that is still there
    /// follows it; one whose prompt is gone (another session, a rewind) ends.
    pub(crate) fn set_conversation_prompts(
        &mut self,
        generation: SessionGeneration,
        fingerprint: PromptsFingerprint,
        prompts: Vec<RecalledPrompt>,
    ) {
        let same_session = self.conversation_prompts.generation == generation;
        let previous = std::mem::replace(
            &mut self.conversation_prompts,
            ConversationPrompts {
                generation,
                fingerprint,
                prompts,
            },
        );
        let Some(Zone::Conversation(place)) = self.walk.as_ref().map(|walk| walk.zone) else {
            return;
        };
        let still_there = same_session
            && previous.prompts.get(place).map(|prompt| &prompt.text)
                == self
                    .conversation_prompts
                    .prompts
                    .get(place)
                    .map(|prompt| &prompt.text);
        if !still_there {
            self.end_prompt_walk();
        } else if previous.prompts[place].cell_index
            != self.conversation_prompts.prompts[place].cell_index
        {
            self.announce_focus(Some(place));
        }
    }

    pub(crate) fn walk_badge(&self) -> Option<WalkBadge> {
        Some(match self.walk.as_ref()?.zone {
            Zone::Conversation(place) => WalkBadge::Turn {
                turn: place + 1,
                total: self.conversation_prompts.prompts.len(),
            },
            Zone::History => WalkBadge::History,
        })
    }

    /// End a walk and bring the stashed draft back. Returns whether there was one.
    pub(crate) fn end_prompt_walk(&mut self) -> bool {
        let Some(walk) = self.walk.take() else {
            return false;
        };
        let was_on_conversation = matches!(walk.zone, Zone::Conversation(_));
        self.history.reset_navigation();
        self.restore_draft(walk.stash);
        if was_on_conversation {
            self.announce_focus(None);
        }
        true
    }

    /// End a walk because what it found was sent: the stashed draft comes back, and nothing is
    /// announced (what was sent is what the app hears next).
    pub(super) fn end_walk_quietly(&mut self) {
        if let Some(walk) = self.walk.take() {
            self.history.reset_navigation();
            self.restore_draft(walk.stash);
        }
    }

    /// The walk's part of a key, before the editor sees it: stepping with `↑`/`↓` at the edges of
    /// the text, sending with `Enter`, leaving with `Esc`. `None` when the key is not the walk's.
    pub(super) fn handle_walk_key(&mut self, key: KeyEvent) -> Option<(InputResult, bool)> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) if self.caret_on_edge_row(true) => {
                self.walk_older().then_some((InputResult::None, true))
            }
            (KeyCode::Down, KeyModifiers::NONE)
                if self.walk.is_some() && self.caret_on_edge_row(false) =>
            {
                self.walk_newer();
                Some((InputResult::None, true))
            }
            // The draft `Ctrl+C` cleared comes back.
            (KeyCode::Down, KeyModifiers::NONE)
                if self.is_empty() && self.cleared_draft.is_some() =>
            {
                let draft = self.cleared_draft.take().unwrap_or_default();
                self.set_text_content(draft);
                Some((InputResult::None, true))
            }
            (KeyCode::Enter, KeyModifiers::NONE | KeyModifiers::ALT) => {
                let Some(Zone::Conversation(place)) = self.walk.as_ref().map(|walk| walk.zone)
                else {
                    return None;
                };
                let files = if key.modifiers == KeyModifiers::ALT {
                    RewindFiles::Keep
                } else {
                    RewindFiles::Restore
                };
                Some(self.send_walked_prompt(place, files))
            }
            // A subagent goes on from the walked turn's answer; the conversation stays as it is.
            (KeyCode::Char('f'), KeyModifiers::CONTROL) => {
                let Some(Zone::Conversation(place)) = self.walk.as_ref().map(|walk| walk.zone)
                else {
                    return None;
                };
                let cell_index = self.conversation_prompts.prompts.get(place)?.cell_index;
                self.end_prompt_walk();
                self.app_event_tx.send(AppEvent::ForkWalkedTurn {
                    generation: self.conversation_prompts.generation,
                    cell_index,
                });
                Some((InputResult::None, true))
            }
            (KeyCode::Esc, KeyModifiers::NONE)
                if self.walk.is_some() && !self.editor_wants_escape() =>
            {
                self.end_prompt_walk();
                Some((InputResult::None, true))
            }
            _ => None,
        }
    }

    /// Whether the caret is on the first (`top`) or last wrapped row of the text, where `↑`/`↓`
    /// leave the text instead of moving in it.
    pub(crate) fn caret_on_edge_row(&self, top: bool) -> bool {
        if self.textarea.is_empty() {
            return true;
        }
        let width = self
            .rendered_body_area
            .get()
            .map_or(u16::MAX, |area| area.width);
        let (row, rows) = self.textarea.cursor_visual_row(width);
        if top { row == 0 } else { row + 1 == rows }
    }

    /// Whether `Esc` means something to the editor right now (leaving insert mode, cancelling a
    /// pending key), so the walk must not take it.
    fn editor_wants_escape(&self) -> bool {
        self.textarea.wants_escape()
    }

    /// Step to the previous prompt. Returns whether there was one to step to.
    fn walk_older(&mut self) -> bool {
        let prompts = self.conversation_prompts.prompts.len();
        match self.walk.as_ref().map(|walk| walk.zone) {
            None if prompts > 0 => {
                self.begin_walk(Zone::Conversation(prompts - 1));
                self.load_walked_prompt(prompts - 1);
                true
            }
            // Nothing sent yet in this conversation: the history of earlier sessions.
            None => match self.history.navigate_up_before_session(&self.app_event_tx) {
                Recall::Exhausted => false,
                recall => {
                    self.begin_walk(Zone::History);
                    self.show_recall(recall);
                    true
                }
            },
            Some(Zone::Conversation(place)) => {
                self.remember_walk_edit(place);
                if place > 0 {
                    self.load_walked_prompt(place - 1);
                } else {
                    let recall = self.history.navigate_up_before_session(&self.app_event_tx);
                    if recall != Recall::Exhausted {
                        if let Some(walk) = self.walk.as_mut() {
                            walk.zone = Zone::History;
                        }
                        self.show_recall(recall);
                        self.announce_focus(None);
                    }
                }
                true
            }
            Some(Zone::History) => {
                let recall = self.history.navigate_up_before_session(&self.app_event_tx);
                self.show_recall(recall);
                true
            }
        }
    }

    fn walk_newer(&mut self) {
        let prompts = self.conversation_prompts.prompts.len();
        match self.walk.as_ref().map(|walk| walk.zone) {
            None => {}
            Some(Zone::Conversation(place)) => {
                self.remember_walk_edit(place);
                if place + 1 < prompts {
                    self.load_walked_prompt(place + 1);
                } else {
                    self.end_prompt_walk();
                }
            }
            Some(Zone::History) => {
                match self
                    .history
                    .navigate_down_before_session(&self.app_event_tx)
                {
                    Recall::Exhausted if prompts > 0 => self.load_walked_prompt(0),
                    Recall::Exhausted => {
                        self.end_prompt_walk();
                    }
                    recall => self.show_recall(recall),
                }
            }
        }
    }

    fn begin_walk(&mut self, zone: Zone) {
        let stash = self.take_draft();
        self.walk = Some(PromptWalk {
            stash,
            zone,
            edits: HashMap::new(),
        });
    }

    /// An entry of earlier sessions: shown now, or when it has been read.
    fn show_recall(&mut self, recall: Recall) {
        match recall {
            Recall::Ready(text) => self.set_text_content(text),
            Recall::Pending => self.set_text_content(String::new()),
            Recall::Exhausted => {}
        }
    }

    fn remember_walk_edit(&mut self, place: usize) {
        let text = self.current_text();
        let original = self
            .conversation_prompts
            .prompts
            .get(place)
            .map(|prompt| prompt.text.as_str());
        if let Some(walk) = self.walk.as_mut() {
            if original == Some(text.as_str()) {
                walk.edits.remove(&place);
            } else {
                walk.edits.insert(place, text);
            }
        }
    }

    /// Put the prompt at `place` (as last edited in this walk) in the composer.
    fn load_walked_prompt(&mut self, place: usize) {
        let Some(prompt) = self.conversation_prompts.prompts.get(place) else {
            return;
        };
        let text = self
            .walk
            .as_ref()
            .and_then(|walk| walk.edits.get(&place))
            .cloned()
            .unwrap_or_else(|| prompt.text.clone());
        if let Some(walk) = self.walk.as_mut() {
            walk.zone = Zone::Conversation(place);
        }
        self.set_text_content(text);
        self.announce_focus(Some(place));
    }

    fn send_walked_prompt(&mut self, place: usize, files: RewindFiles) -> (InputResult, bool) {
        let Some(cell_index) = self
            .conversation_prompts
            .prompts
            .get(place)
            .map(|prompt| prompt.cell_index)
        else {
            return (InputResult::None, true);
        };
        let text = self.expand_pending_pastes(self.current_text());
        let text = text.trim().to_owned();
        if text.is_empty() {
            return (InputResult::None, true);
        }
        let selection = RewindSelection {
            generation: self.conversation_prompts.generation,
            cell_index,
            turn: place + 1,
            text,
            files,
        };
        // The walk ends without announcing it: the rewind is what the app hears next.
        self.end_walk_quietly();
        self.app_event_tx.send(AppEvent::RewindSelection(selection));
        (InputResult::None, true)
    }

    fn announce_focus(&self, place: Option<usize>) {
        let cell = place.and_then(|place| {
            self.conversation_prompts
                .prompts
                .get(place)
                .map(|prompt| prompt.cell_index)
        });
        self.app_event_tx.send(AppEvent::FocusChanged {
            generation: self.conversation_prompts.generation,
            cell,
        });
    }
}
