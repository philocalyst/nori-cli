//! Keys, paste and pointer: what the prompt and the panes do with them.
//!
//! | input | effect |
//! |---|---|
//! | text, Backspace, Ctrl-W, Ctrl-U | edit the query (Ctrl-U clears it) |
//! | Up / Down, Ctrl-P / Ctrl-N | move the selection, wrapping |
//! | PageUp / PageDown, Home / End | page, first, last |
//! | Tab | show or hide the preview |
//! | Space | toggle the row, in a multi-select picker with an empty query |
//! | Ctrl-D / Ctrl-F, Ctrl-B | scroll the preview |
//! | Alt-N / Alt-P | next / previous match in the preview |
//! | Alt-`<key>` | an action bound to the selected item |
//! | Enter | submit the selected item |
//! | Esc | clear the query, then cancel; Ctrl-C interrupts |
//! | wheel | list: move; preview: scroll. Click: submit that row |

use ratatui::crossterm::event::KeyCode;
use ratatui::crossterm::event::KeyEvent;
use ratatui::crossterm::event::KeyEventKind;
use ratatui::crossterm::event::KeyModifiers;
use ratatui::crossterm::event::MouseButton;
use ratatui::crossterm::event::MouseEvent;
use ratatui::crossterm::event::MouseEventKind;
use ratatui::layout::Position;

use super::item::HelixItem;
use super::state::HelixState;

/// What the host must do after an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelixEvent<Id> {
    /// Nothing changed.
    Idle,
    /// State changed; draw again.
    Redraw,
    /// The user chose this item.
    Submit(Id),
    /// The user invoked a bound action on this item.
    Action(&'static str, Id),
    /// The user chose these items in a multi-select picker.
    SubmitMany(Vec<Id>),
    /// Shift-Tab: the picker's secondary action, which host pickers may bind.
    Secondary,
    /// The user finished the text an action asked for.
    Composed(&'static str, Id, String),
    Cancel,
    /// Ctrl-C: the user wants out of more than this picker. A host that can quit does;
    /// others treat it as `Cancel`.
    Interrupt,
}

const PREVIEW_STEP: isize = 6;
const WHEEL_STEP: isize = 3;

impl<I: HelixItem> HelixState<I> {
    pub fn on_key(&mut self, key: KeyEvent) -> HelixEvent<I::Id> {
        if key.kind == KeyEventKind::Release {
            return HelixEvent::Idle;
        }
        self.refresh();
        if self.compose.is_some() {
            return self.on_compose_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                if self.searching {
                    self.searching = false;
                    self.set_query(String::new());
                } else if self.query.is_empty() {
                    return HelixEvent::Cancel;
                } else {
                    self.set_query(String::new());
                }
            }
            KeyCode::Char('f') if ctrl && self.config.on_demand && !self.searching => {
                self.searching = true;
            }
            KeyCode::Char('c') if ctrl => return HelixEvent::Interrupt,
            KeyCode::Enter => return self.submit(),
            KeyCode::Up => self.step(-1, true),
            KeyCode::Down => self.step(1, true),
            KeyCode::Char('p') if ctrl => self.step(-1, true),
            KeyCode::Char('n') if ctrl => self.step(1, true),
            KeyCode::PageUp => self.step(-self.page_rows(), false),
            KeyCode::PageDown => self.step(self.page_rows(), false),
            KeyCode::Home => self.select_edge(false),
            KeyCode::End => self.select_edge(true),
            KeyCode::BackTab => return HelixEvent::Secondary,
            KeyCode::Tab if key.modifiers.contains(KeyModifiers::SHIFT) => {
                return HelixEvent::Secondary;
            }
            KeyCode::Tab if self.config.preview => self.preview_hidden = !self.preview_hidden,
            KeyCode::Char(' ') if self.config.multi && self.query.is_empty() && !alt => {
                let Some(id) = self.selected else {
                    return HelixEvent::Idle;
                };
                match self.checked.iter().position(|checked| *checked == id) {
                    Some(at) => {
                        self.checked.remove(at);
                    }
                    None => self.checked.push(id),
                }
            }
            KeyCode::Char('d' | 'f') if ctrl => self.preview_scroll += PREVIEW_STEP,
            KeyCode::Char('b') if ctrl => self.preview_scroll -= PREVIEW_STEP,
            KeyCode::Char('n') if alt => self.jump_hit(1),
            KeyCode::Char('p') if alt => self.jump_hit(-1),
            KeyCode::Char('u') if ctrl => {
                if self.query.is_empty() {
                    return HelixEvent::Idle;
                }
                self.set_query(String::new());
            }
            KeyCode::Char('w') if ctrl => {
                let kept = self
                    .query
                    .trim_end()
                    .trim_end_matches(|character: char| !character.is_whitespace())
                    .len();
                if kept == self.query.len() {
                    return HelixEvent::Idle;
                }
                let mut query = std::mem::take(&mut self.query);
                query.truncate(kept);
                self.set_query(query);
            }
            KeyCode::Backspace => {
                let mut query = std::mem::take(&mut self.query);
                if query.pop().is_none() {
                    return HelixEvent::Idle;
                }
                self.set_query(query);
            }
            KeyCode::Char(character) if alt && !ctrl => return self.row_action(character, true),
            KeyCode::Char(character) if !ctrl => {
                let typing = self.config.searchable && (!self.config.on_demand || self.searching);
                if typing {
                    let mut query = std::mem::take(&mut self.query);
                    query.push(character);
                    self.set_query(query);
                } else {
                    return self.plain_key(character);
                }
            }
            _ => return HelixEvent::Idle,
        }
        HelixEvent::Redraw
    }

    /// Enter: what choosing the selection means for this picker.
    fn submit(&mut self) -> HelixEvent<I::Id> {
        let Some(id) = self.selected else {
            return HelixEvent::Idle;
        };
        if !self.selected().is_some_and(HelixItem::enabled) {
            return HelixEvent::Idle;
        }
        if let Some(verb) = self.selected().and_then(HelixItem::asks) {
            let label = self
                .selected()
                .map(HelixItem::title_text)
                .unwrap_or_default();
            self.begin_compose("ask", verb, label, String::new(), id);
            return HelixEvent::Redraw;
        }
        if self.config.multi && !self.checked.is_empty() {
            return HelixEvent::SubmitMany(self.checked.clone());
        }
        HelixEvent::Submit(id)
    }

    fn on_compose_key(&mut self, key: KeyEvent) -> HelixEvent<I::Id> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.compose = None,
            KeyCode::Char('c') if ctrl => self.compose = None,
            KeyCode::Enter => {
                let Some(compose) = self
                    .compose
                    .take_if(|compose| !compose.text.trim().is_empty())
                else {
                    return HelixEvent::Idle;
                };
                return HelixEvent::Composed(compose.action, compose.id, compose.text);
            }
            KeyCode::Backspace => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.text.pop();
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.text.clear();
                }
            }
            KeyCode::Char(character) if !ctrl => {
                if let Some(compose) = self.compose.as_mut() {
                    compose.text.push(character);
                }
            }
            _ => return HelixEvent::Idle,
        }
        HelixEvent::Redraw
    }

    pub fn on_paste(&mut self, text: &str) -> HelixEvent<I::Id> {
        if let Some(compose) = self.compose.as_mut() {
            compose
                .text
                .extend(text.chars().filter(|character| !character.is_control()));
            return HelixEvent::Redraw;
        }
        let mut query = std::mem::take(&mut self.query);
        query.extend(text.chars().filter(|character| !character.is_control()));
        self.set_query(query);
        HelixEvent::Redraw
    }

    pub fn on_mouse(&mut self, mouse: MouseEvent) -> HelixEvent<I::Id> {
        let Some(layout) = self.layout else {
            return HelixEvent::Idle;
        };
        self.refresh();
        let position = Position::new(mouse.column, mouse.row);
        let in_list = layout.list.contains(position);
        let in_preview = layout.preview.is_some_and(|pane| pane.contains(position));
        match mouse.kind {
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = mouse.kind == MouseEventKind::ScrollUp;
                if in_list {
                    self.step(if up { -1 } else { 1 }, false);
                } else if in_preview {
                    self.preview_scroll += if up { -WHEEL_STEP } else { WHEEL_STEP };
                } else {
                    return HelixEvent::Idle;
                }
                HelixEvent::Redraw
            }
            MouseEventKind::Down(MouseButton::Left) if in_list => {
                let row = self.drawn.iter().find(|(_, y, height)| {
                    mouse.row >= *y && mouse.row < y.saturating_add(*height)
                });
                match row.and_then(|(found, _, _)| self.matches.get(*found)) {
                    Some(found) => {
                        let id = self.items[found.index].id();
                        self.set_selected(Some(id));
                        HelixEvent::Submit(id)
                    }
                    None => HelixEvent::Idle,
                }
            }
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => HelixEvent::Idle,
        }
    }

    /// A plain key while no query is being typed: open the query, move, choose by number, or
    /// run a row action bound to it.
    fn plain_key(&mut self, character: char) -> HelixEvent<I::Id> {
        if self.config.searchable && matches!(character, '/' | 'f') {
            self.searching = true;
            return HelixEvent::Redraw;
        }
        if self
            .actions
            .iter()
            .any(|action| action.plain && action.key == character)
        {
            return self.row_action(character, false);
        }
        match character {
            'j' => self.step(1, true),
            'k' => self.step(-1, true),
            digit if self.config.numbered && digit.is_ascii_digit() => {
                return self.choose_number(digit);
            }
            _ => return HelixEvent::Idle,
        }
        HelixEvent::Redraw
    }

    /// Choose the enabled row numbered by `digit`.
    fn choose_number(&mut self, digit: char) -> HelixEvent<I::Id> {
        let Some(number) = digit.to_digit(10).and_then(|n| usize::try_from(n).ok()) else {
            return HelixEvent::Idle;
        };
        let id = number.checked_sub(1).and_then(|skip| {
            self.matches
                .iter()
                .map(|found| &self.items[found.index])
                .filter(|item| item.enabled())
                .nth(skip)
                .map(HelixItem::id)
        });
        match id {
            Some(id) => {
                self.set_selected(Some(id));
                HelixEvent::Submit(id)
            }
            None => HelixEvent::Idle,
        }
    }

    /// The action bound to `character` (with Alt, or plain) on the selected row.
    fn row_action(&mut self, character: char, alt: bool) -> HelixEvent<I::Id> {
        let action = self
            .actions
            .iter()
            .find(|action| action.key == character && action.plain != alt)
            .copied();
        match (action, self.selected) {
            (Some(action), Some(id)) => match action.compose {
                Some(verb) => {
                    let target = self
                        .selected()
                        .map(HelixItem::title_text)
                        .unwrap_or_default();
                    self.begin_compose(action.name, verb, target, String::new(), id);
                    HelixEvent::Redraw
                }
                None => HelixEvent::Action(action.name, id),
            },
            (Some(_) | None, _) => HelixEvent::Idle,
        }
    }

    fn page_rows(&self) -> isize {
        isize::try_from(self.page).unwrap_or(isize::MAX).max(1)
    }
}
