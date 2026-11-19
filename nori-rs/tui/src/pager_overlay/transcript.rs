//! The transcript overlay (Ctrl+T): every cell of the conversation, with a selected message
//! whose actions (edit, rewind, fork) open on Enter or a double click.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::CachedRenderable;
use super::CellRenderable;
use super::KEY_CTRL_C;
use super::KEY_CTRL_T;
use super::KEY_ENTER;
use super::KEY_ESC;
use super::KEY_Q;
use super::PagerView;
use super::message_menu::MenuOutcome;
use super::message_menu::MessageAction;
use super::message_menu::MessageChoice;
use super::message_menu::MessageMenu;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::render::Insets;
use crate::render::renderable::InsetRenderable;
use crate::render::renderable::Renderable;
use crate::style::user_message_style;
use crate::tui;
use crate::tui::TuiEvent;

/// Two presses of the left button on one message within this long are a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);
/// Rows one notch of the wheel scrolls.
const WHEEL_ROWS: usize = 3;
const KEY_PREVIOUS_MESSAGE: KeyBinding = key_hint::plain(KeyCode::Char('['));
const KEY_NEXT_MESSAGE: KeyBinding = key_hint::plain(KeyCode::Char(']'));
const KEY_PREVIOUS_RESPONSE: KeyBinding = key_hint::plain(KeyCode::Char('['));
const KEY_NEXT_RESPONSE: KeyBinding = key_hint::plain(KeyCode::Char(']'));

pub(crate) struct TranscriptOverlay {
    pub(super) view: PagerView,
    pub(super) cells: Vec<Arc<dyn HistoryCell>>,
    pub(super) highlight_cell: Option<usize>,
    response_focus: bool,
    /// The last press of the left button, and the cell it was on, to tell a double click.
    last_click: Option<(Instant, usize)>,
    /// The actions offered for the selected message, while they are open.
    menu: Option<MessageMenu>,
    /// The action the user chose, for the app to carry out.
    chosen: Option<MessageAction>,
    is_done: bool,
}

impl TranscriptOverlay {
    pub(crate) fn new(transcript_cells: Vec<Arc<dyn HistoryCell>>) -> Self {
        Self {
            view: PagerView::new(
                Self::render_cells(&transcript_cells, None),
                "T R A N S C R I P T".to_string(),
                usize::MAX,
            ),
            cells: transcript_cells,
            highlight_cell: None,
            response_focus: false,
            last_click: None,
            menu: None,
            chosen: None,
            is_done: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn focus_responses(&mut self) {
        self.response_focus = true;
        self.step_response(true);
    }

    fn step_response(&mut self, older: bool) {
        let mut responses = self.cells.iter().enumerate().filter_map(|(index, cell)| {
            ((cell.as_any().is::<AgentMessageCell>() && !cell.is_stream_continuation())
                || cell.as_any().is::<AgentMarkdownCell>())
            .then_some(index)
        });
        let next = match (self.highlight_cell, older) {
            (None, _) => responses.next_back(),
            (Some(current), true) => responses.rev().find(|index| *index < current),
            (Some(current), false) => responses.find(|index| *index > current),
        };
        if next.is_some() {
            self.set_highlight_cell(next);
        }
    }

    fn render_cells(
        cells: &[Arc<dyn HistoryCell>],
        highlight_cell: Option<usize>,
    ) -> Vec<Box<dyn Renderable>> {
        Self::render_cells_from(cells, 0, highlight_cell)
    }

    /// The renderables of `cells[start..]`.
    fn render_cells_from(
        cells: &[Arc<dyn HistoryCell>],
        start: usize,
        highlight_cell: Option<usize>,
    ) -> Vec<Box<dyn Renderable>> {
        cells
            .iter()
            .enumerate()
            .skip(start)
            .map(|(i, c)| Self::cell_renderable(c, i, highlight_cell))
            .collect()
    }

    fn cell_renderable(
        c: &Arc<dyn HistoryCell>,
        i: usize,
        highlight_cell: Option<usize>,
    ) -> Box<dyn Renderable> {
        let base = if c.as_any().is::<UserHistoryCell>() {
            user_message_style()
        } else {
            Style::default()
        };
        let mut cell_renderable = Box::new(CachedRenderable::new(CellRenderable {
            cell: c.clone(),
            style: if highlight_cell == Some(i) {
                base.reversed()
            } else {
                base
            },
        })) as Box<dyn Renderable>;
        if !c.is_stream_continuation() && i > 0 {
            cell_renderable = Box::new(InsetRenderable::new(
                cell_renderable,
                Insets::tlbr(1, 0, 0, 0),
            ));
        }
        cell_renderable
    }

    pub(crate) fn insert_cell(&mut self, cell: Arc<dyn HistoryCell>) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        let index = self.cells.len();
        let renderable = Self::cell_renderable(&cell, index, self.highlight_cell);
        self.cells.push(cell);
        self.view.push_renderable(renderable);
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    pub(crate) fn replace_cells(
        &mut self,
        range: std::ops::Range<usize>,
        replacement: Arc<dyn HistoryCell>,
    ) {
        let follow_bottom = self.view.is_scrolled_to_bottom();
        let start = range.start;
        self.cells.splice(range, std::iter::once(replacement));
        // Items before the replaced range keep their layout; only the tail is rebuilt.
        let tail = Self::render_cells_from(&self.cells, start, self.highlight_cell);
        self.view.replace_from(start, tail);
        if follow_bottom {
            self.view.scroll_offset = usize::MAX;
        }
    }

    pub(crate) fn set_highlight_cell(&mut self, cell: Option<usize>) {
        let previous = std::mem::replace(&mut self.highlight_cell, cell);
        // Only the style of the two cells involved changes, never a height.
        for index in previous.into_iter().chain(cell) {
            if let Some(cell) = self.cells.get(index) {
                let renderable = Self::cell_renderable(cell, index, self.highlight_cell);
                self.view.renderables[index] = renderable;
            }
        }
        if let Some(idx) = self.highlight_cell {
            self.view.scroll_chunk_into_view(idx);
        }
    }

    /// The action the user chose for a message, once.
    pub(crate) fn take_chosen(&mut self) -> Option<MessageAction> {
        self.chosen.take()
    }

    /// Open the actions of the selected message.
    fn open_menu(&mut self) {
        if let Some(cell) = self.highlight_cell
            && let Some(selected) = self.cells.get(cell)
        {
            let is_prompt = selected.as_any().is::<UserHistoryCell>();
            self.menu = Some(MessageMenu::for_cell(cell, is_prompt));
        }
    }

    /// Select the message before (`older`) or after the selected one: a prompt, or the start of
    /// an answer. With nothing selected, the newest message.
    fn step_message(&mut self, older: bool) {
        let is_message = |cell: &Arc<dyn HistoryCell>| {
            cell.as_any().is::<UserHistoryCell>()
                || ((cell.as_any().is::<AgentMessageCell>() && !cell.is_stream_continuation())
                    || cell.as_any().is::<AgentMarkdownCell>())
        };
        let mut messages = self
            .cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| is_message(cell))
            .map(|(index, _)| index);
        let next = match (self.highlight_cell, older) {
            (None, _) => messages.next_back(),
            (Some(current), true) => messages.rev().find(|index| *index < current),
            (Some(current), false) => messages.find(|index| *index > current),
        };
        if next.is_some() {
            self.set_highlight_cell(next);
        }
    }

    /// Act on a key, without a terminal to page by. Returns whether the key was the overlay's.
    pub(super) fn on_key(&mut self, key_event: KeyEvent) -> bool {
        if let Some(menu) = self.menu.as_mut() {
            match menu.handle_key(key_event) {
                MenuOutcome::Open => {}
                MenuOutcome::Closed => self.menu = None,
                MenuOutcome::Chosen(action) => {
                    self.menu = None;
                    self.chosen = Some(action);
                }
            }
            return true;
        }
        if self.response_focus {
            match key_event {
                e if KEY_PREVIOUS_RESPONSE.is_press(e) => self.step_response(true),
                e if KEY_NEXT_RESPONSE.is_press(e) => self.step_response(false),
                e if KEY_ENTER.is_press(e) => {
                    if let Some(cell) = self.highlight_cell {
                        self.chosen = Some(MessageAction {
                            cell,
                            choice: MessageChoice::RollbackThrough,
                        });
                    }
                }
                e if super::KEY_SPACE.is_press(e) => {
                    if let Some(cell) = self.highlight_cell {
                        self.chosen = Some(MessageAction {
                            cell,
                            choice: MessageChoice::ForkSubagent,
                        });
                    }
                }
                e if KEY_ESC.is_press(e) || KEY_Q.is_press(e) || KEY_CTRL_C.is_press(e) => {
                    self.is_done = true
                }
                _ => return false,
            }
            return true;
        }
        match key_event {
            e if KEY_PREVIOUS_MESSAGE.is_press(e) => self.step_message(true),
            e if KEY_NEXT_MESSAGE.is_press(e) => self.step_message(false),
            e if KEY_ENTER.is_press(e) && self.highlight_cell.is_some() => self.open_menu(),
            e if KEY_Q.is_press(e) || KEY_CTRL_C.is_press(e) || KEY_CTRL_T.is_press(e) => {
                self.is_done = true;
            }
            _ => return false,
        }
        true
    }

    /// Act on the mouse at `now`: the wheel scrolls, a click selects the message under it, and a
    /// second click on it soon after opens its actions. Returns whether anything changed.
    pub(super) fn on_mouse(&mut self, mouse: MouseEvent, now: Instant) -> bool {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.view.scroll_offset = self.view.scroll_offset.saturating_sub(WHEEL_ROWS);
                true
            }
            MouseEventKind::ScrollDown => {
                self.view.scroll_offset = self.view.scroll_offset.saturating_add(WHEEL_ROWS);
                true
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(cell) = self.view.item_at(mouse.row) else {
                    self.menu = None;
                    return true;
                };
                let double = self.last_click.is_some_and(|(at, clicked)| {
                    clicked == cell && now.saturating_duration_since(at) <= DOUBLE_CLICK
                });
                self.menu = None;
                self.set_highlight_cell(Some(cell));
                if double {
                    self.last_click = None;
                    self.open_menu();
                } else {
                    self.last_click = Some((now, cell));
                }
                true
            }
            _ => false,
        }
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(1);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 1);
        self.view.render(top, buf);
        crate::bottom_pane::render_mode_row(
            bottom,
            buf,
            crate::bottom_pane::StatusMode::Transcript,
        );
        if let Some(menu) = &self.menu {
            let message = self.view.item_rows(menu.cell());
            menu.render(self.view.content_area(top), message, buf);
        }
    }

    pub(crate) fn handle_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> std::io::Result<()> {
        match event {
            TuiEvent::Key(key_event) => {
                if self.on_key(key_event) {
                    tui.frame_requester().schedule_frame();
                    Ok(())
                } else {
                    self.view.handle_key_event(tui, key_event)
                }
            }
            TuiEvent::Mouse(mouse) => {
                if self.on_mouse(mouse, Instant::now()) {
                    tui.frame_requester().schedule_frame();
                }
                Ok(())
            }
            TuiEvent::Draw => {
                tui.draw_overlay(|frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
                Ok(())
            }
            TuiEvent::Paste(_) => Ok(()),
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }

    /// The cell the open menu belongs to.
    #[cfg(test)]
    pub(super) fn menu_cell(&self) -> Option<usize> {
        self.menu.as_ref().map(MessageMenu::cell)
    }
}
