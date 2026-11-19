//! Clicks, drags and the wheel, when the app has asked for them (see `draw_frame`).

use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;

use super::super::App;
use crate::subagent_switcher::Target;
use crate::tui::Tui;

use super::super::session_view::CARD_PREFIX;

/// Rows one notch of the wheel scrolls.
const WHEEL_ROWS: isize = 3;

impl App {
    pub(in crate::app) fn handle_mouse(&mut self, tui: &mut Tui, mouse: MouseEvent) {
        if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
            self.tool_gesture.cancel();
        }
        if self.chat_widget.has_active_overlay_or_popup() {
            self.chat_widget.handle_popup_mouse(mouse);
            return;
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => self.mouse_down(tui, mouse),
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag(tui, mouse),
            MouseEventKind::Up(MouseButton::Left) => self.mouse_up(tui, mouse),
            MouseEventKind::ScrollUp => self.mouse_wheel(tui, WHEEL_ROWS),
            MouseEventKind::ScrollDown => self.mouse_wheel(tui, -WHEEL_ROWS),
            MouseEventKind::Down(_)
            | MouseEventKind::Up(_)
            | MouseEventKind::Drag(_)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
    }

    /// The tool card under a point: one of the stage's, or of a projected view.
    fn tool_under(&self, tui: &Tui, column: u16, row: u16) -> Option<String> {
        self.chat_widget
            .transcript_tool_at(column, row)
            .or_else(|| tui.card_at(row).map(|cell| format!("{CARD_PREFIX}{cell}")))
    }

    fn mouse_down(&mut self, tui: &mut Tui, mouse: MouseEvent) {
        let (column, row) = (mouse.column, mouse.row);
        if let Some(call_id) = self.tool_under(tui, column, row) {
            self.tool_gesture
                .press(self.session_view.focused(), call_id, column, row);
            self.session_view.start_drag(row);
        } else if let Some(child) = self.chat_widget.subagent_at(column, row) {
            self.focus_subagent_from_mouse(tui, child);
        } else if self.chat_widget.plan_drawer_contains(column, row) {
            self.chat_widget.toggle_plan_drawer();
            self.plan_drawer_mode = self.chat_widget.plan_drawer_mode();
            tui.frame_requester().schedule_frame();
        } else if self.chat_widget.place_composer_cursor(column, row) {
            self.session_view.end_drag();
        } else if self.chat_widget.is_chat_content_row(row) {
            self.session_view.start_drag(row);
        } else {
            self.session_view.end_drag();
        }
    }

    fn mouse_drag(&mut self, tui: &mut Tui, mouse: MouseEvent) {
        self.tool_gesture
            .drag(self.session_view.focused(), mouse.column, mouse.row);
        let delta = self.session_view.drag(mouse.row);
        if delta == 0 {
            return;
        }
        if self.session_view.focused() == Target::Main {
            self.scroll_main_session(tui, delta);
        } else {
            self.session_view.scroll(delta);
            tui.frame_requester().schedule_frame();
        }
    }

    fn mouse_up(&mut self, tui: &mut Tui, mouse: MouseEvent) {
        let hit = self.tool_under(tui, mouse.column, mouse.row);
        if let Some(call_id) = self
            .tool_gesture
            .release(self.session_view.focused(), hit.as_deref())
        {
            self.session_view.end_drag();
            self.toggle_tool(tui, call_id);
            return;
        }
        self.session_view.end_drag();
    }

    fn mouse_wheel(&mut self, tui: &mut Tui, rows: isize) {
        match self.session_view.focused() {
            Target::Subagent(_) => {
                self.session_view.scroll(rows);
                tui.frame_requester().schedule_frame();
            }
            Target::Main => self.scroll_main_session(tui, rows),
        }
    }
}
