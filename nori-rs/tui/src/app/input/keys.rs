//! Keys for the composer and what sits around it.
//!
//! The composer's own editing belongs to the composer. This is the part that belongs to the app:
//! the configurable hotkeys, how Esc behaves (interrupt a running turn, stop or leave a child,
//! focus the last prompt) and how a focused prompt is entered (`Esc` on an empty composer, `[`
//! in normal mode).

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use nori_config::HotkeyAction;
use nori_config::KeyAction;
use nori_config::KeyMode;

use super::super::App;
use super::super::focus::is_prompt;
use super::InputMode;
use crate::app_event::AppEvent;
use crate::nori::hotkey_match::matches_binding;
use crate::subagent_switcher::Status;
use crate::subagent_switcher::Target;
use crate::tui::Tui;

impl App {
    pub(in crate::app) async fn handle_key(&mut self, tui: &mut Tui, key: KeyEvent) {
        if self.buffer_key_for_child_history(&key) {
            return;
        }
        if self.input_mode() == InputMode::Focus && self.handle_focus_key(tui, key) {
            return;
        }
        let pressed = key.kind == KeyEventKind::Press;
        // Configurable hotkeys go first, but only when no popup or view is active: a popup
        // captures the key (the hotkey picker in rebinding mode, for one).
        if pressed && !self.chat_widget.has_active_popup() && self.run_hotkey(tui, &key) {
            return;
        }
        if pressed && self.input_mode() == InputMode::Composer && self.enter_focus_with(tui, &key) {
            return;
        }
        match key.kind {
            KeyEventKind::Release => {}
            KeyEventKind::Press | KeyEventKind::Repeat if key.code == KeyCode::Esc => {
                self.escape(tui, key);
            }
            KeyEventKind::Press | KeyEventKind::Repeat => self.chat_widget.handle_key_event(key),
        }
    }

    /// Keys typed while a child's journal loads for focusing wait for it, and are replayed then.
    fn buffer_key_for_child_history(&mut self, key: &KeyEvent) -> bool {
        let Some(pending) = self.pending_child_focus.as_mut() else {
            return false;
        };
        if self.session_view.focused() != Target::Subagent(pending.key)
            || pending.generation != self.chat_widget.session_generation()
            || self.chat_widget.subagent_session(pending.key).as_ref() != Some(&pending.session_id)
        {
            self.pending_child_focus = None;
            return false;
        }
        if key.kind != KeyEventKind::Press {
            return false;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                self.pending_child_focus = None;
                true
            }
            (KeyCode::Up | KeyCode::Down | KeyCode::Enter, KeyModifiers::NONE)
            | (KeyCode::Char('[' | ']' | 'j' | 'k' | ' ' | 'f'), KeyModifiers::NONE) => {
                pending.keys.push(*key);
                true
            }
            _ => {
                self.pending_child_focus = None;
                false
            }
        }
    }

    /// The hotkeys the user can rebind. Returns whether `key` was one.
    fn run_hotkey(&mut self, tui: &mut Tui, key: &KeyEvent) -> bool {
        let binding = |action| self.hotkey_config.binding_for(action);
        if matches_binding(binding(HotkeyAction::OpenTranscript), key) {
            if let Target::Subagent(child) = self.session_view.focused() {
                self.request_child_history(tui, child, false);
            } else {
                self.open_transcript_overlay(tui);
            }
        } else if matches_binding(binding(HotkeyAction::OpenEditor), key) {
            self.open_external_editor(tui);
        } else if matches_binding(binding(HotkeyAction::CommitChanges), key) {
            self.app_event_tx
                .send(AppEvent::CommitChanges { automatic: false });
        } else if matches_binding(binding(HotkeyAction::SwitchSubagent), key) {
            self.app_event_tx.send(AppEvent::OpenActorPanel);
        } else if matches_binding(binding(HotkeyAction::TogglePlanDrawer), key) {
            self.chat_widget.toggle_plan_drawer();
            self.plan_drawer_mode = self.chat_widget.plan_drawer_mode();
            tui.frame_requester().schedule_frame();
        } else {
            return false;
        }
        true
    }

    /// The keys that walk back to the prompts of the conversation shown (`Esc` on an empty
    /// composer while idle; `[` in normal mode), or load a child's journal to do it there.
    /// Which keys they are is the keymap's; returns whether `key` did.
    fn enter_focus_with(&mut self, tui: &mut Tui, key: &KeyEvent) -> bool {
        let Some(action) =
            self.config
                .keymap
                .find(KeyMode::Composer, &self.hotkey_config, |binding| {
                    matches_binding(binding, key)
                })
        else {
            return false;
        };
        let idle = self.chat_widget.is_normal_backtrack_mode();
        let allowed = match action {
            KeyAction::EnterFocus => {
                idle && self.chat_widget.composer_is_empty() && !self.chat_widget.is_task_running()
            }
            // `[` walks back in normal mode, and from an empty composer while subagents run (when
            // Esc is for stopping them).
            KeyAction::EnterFocusBracket => {
                idle && (self.chat_widget.is_composer_in_vim_normal_mode()
                    || self.chat_widget.composer_is_empty()
                        && self.chat_widget.subagents_stoppable())
            }
            _ => false,
        };
        if !allowed {
            return false;
        }
        // Subagents run while the agent is idle: Esc is for them first, `[` still rewinds.
        if action == KeyAction::EnterFocus
            && self.session_view.focused() == Target::Main
            && self.chat_widget.subagents_stoppable()
        {
            if self.chat_widget.stop_subagents_armed() {
                self.chat_widget.set_status_note(None);
                self.chat_widget.stop_background_subagents();
            } else {
                self.chat_widget.arm_stop_subagents();
            }
            return true;
        }
        if let Target::Subagent(child) = self.session_view.focused() {
            if action == KeyAction::EnterFocusBracket {
                self.request_child_history(tui, child, true);
                return true;
            }
            return false;
        }
        if !self.transcript_cells.iter().any(is_prompt) {
            return false;
        }
        self.begin_focus(tui, Vec::new(), action == KeyAction::EnterFocusBracket);
        true
    }

    /// Esc: stop what is running, or leave where the user is, or walk back to the last prompt.
    fn escape(&mut self, tui: &mut Tui, key: KeyEvent) {
        if self.chat_widget.has_active_overlay_or_popup() {
            self.chat_widget.handle_key_event(key);
            return;
        }
        match self.session_view.focused() {
            // The agent ignored a cancel: Esc again is the deliberate way to stop it.
            Target::Main if self.chat_widget.cancel_ignored() => {
                self.chat_widget
                    .submit_harness_action(crate::app_event::HarnessAction::StopAgent);
                self.chat_widget.set_status_note(None);
            }
            Target::Main if self.chat_widget.is_task_running() => {
                self.chat_widget.handle_key_event(key);
            }
            Target::Main => self.chat_widget.handle_key_event(key),
            Target::Subagent(child) => self.escape_child(tui, child, key),
        }
    }

    /// Esc in a child's view stops the child if it runs, lets the composer have it when it is
    /// being edited, and otherwise returns to the parent.
    fn escape_child(
        &mut self,
        tui: &mut Tui,
        child: crate::subagent_switcher::SubagentKey,
        key: KeyEvent,
    ) {
        let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
        let running = roster
            .entry(Target::Subagent(child))
            .is_some_and(|entry| entry.status == Status::Running);
        if running {
            let (Some(handle), Some(session_id)) = (
                self.chat_widget.harness_handle(),
                self.chat_widget.subagent_session(child),
            ) else {
                tui.show_toast("This child has no session available to stop.".to_owned());
                return;
            };
            let tx = self.app_event_tx.clone();
            tokio::spawn(async move {
                if let Err(error) = handle.cancel_subagent(session_id).await {
                    tx.send(AppEvent::TransientNotice(error.to_string()));
                }
            });
        } else if self.chat_widget.composer_cursor_style()
            == crossterm::cursor::SetCursorStyle::SteadyBar
        {
            self.chat_widget.handle_key_event(key);
        } else {
            let target = self
                .chat_widget
                .subagent_parent(child)
                .map(Target::Subagent)
                .map(|target| roster.resolve(target))
                .filter(|target| roster.entry(*target).is_some())
                .unwrap_or(Target::Main);
            self.focus_session_view(tui, target);
        }
    }
}
