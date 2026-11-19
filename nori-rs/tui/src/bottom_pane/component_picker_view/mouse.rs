//! Tap and swipe interaction against the picker's last rendered geometry.

use std::cell::Cell;
use std::cell::RefCell;

use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;
use nori_tui_components::PickerAction;
use nori_tui_components::PickerHit;
use nori_tui_components::PickerHitTarget;
use ratatui::layout::Position;
use ratatui::layout::Rect;

use super::ComponentPickerView;

#[derive(Default)]
pub(super) struct PickerMouse {
    pub(super) regions: RefCell<Vec<PickerHit<String>>>,
    pub(super) area: Cell<Rect>,
    press: Option<(i32, i32, Option<PickerHitTarget<String>>)>,
    last_row: i32,
    dragged: bool,
}

impl ComponentPickerView {
    pub(super) fn on_mouse_event(&mut self, mouse: MouseEvent) -> bool {
        let position = Position::new(mouse.column, mouse.row);
        let hit = self
            .mouse
            .regions
            .borrow()
            .iter()
            .find(|hit| hit.area.contains(position))
            .map(|hit| hit.target.clone());
        let mut changed = false;
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.handle_action(PickerAction::MoveUp);
                changed = true;
            }
            MouseEventKind::ScrollDown => {
                self.handle_action(PickerAction::MoveDown);
                changed = true;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if self.mouse.area.get().contains(position) {
                    self.mouse.press = Some((i32::from(mouse.column), i32::from(mouse.row), hit));
                    self.mouse.last_row = i32::from(mouse.row);
                    self.mouse.dragged = false;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((column, row, _)) = self.mouse.press.as_ref() {
                    self.mouse.dragged |= (i32::from(mouse.column) - column).abs() > 1
                        || (i32::from(mouse.row) - row).abs() > 1;
                    if self.mouse.dragged {
                        let delta = i32::from(mouse.row) - self.mouse.last_row;
                        changed |= delta != 0;
                        for _ in 0..delta.abs().min(32) {
                            self.handle_action(if delta > 0 {
                                PickerAction::MoveUp
                            } else {
                                PickerAction::MoveDown
                            });
                        }
                    }
                    self.mouse.last_row = i32::from(mouse.row);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some((_, _, Some(target))) = self.mouse.press.take()
                    && !self.mouse.dragged
                    && hit.as_ref() == Some(&target)
                {
                    changed = true;
                    match target {
                        PickerHitTarget::Item(key) => {
                            let selected = self.state.visible_indices().into_iter().find(|index| {
                                self.state.items[*index].key == key
                                    && !self.state.items[*index].disabled
                            });
                            if let Some(index) = selected {
                                self.state.selected_index = Some(index);
                                self.handle_action(PickerAction::Submit);
                            }
                        }
                        PickerHitTarget::Category(category) => {
                            self.state.active_category = category;
                            self.handle_action(PickerAction::First);
                        }
                        PickerHitTarget::Search => self.handle_action(PickerAction::ActivateSearch),
                        PickerHitTarget::Close => self.handle_action(PickerAction::Cancel),
                    }
                }
            }
            MouseEventKind::Down(MouseButton::Right | MouseButton::Middle)
            | MouseEventKind::Up(MouseButton::Right | MouseButton::Middle)
            | MouseEventKind::Drag(MouseButton::Right | MouseButton::Middle)
            | MouseEventKind::Moved
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {}
        }
        changed
    }
}
