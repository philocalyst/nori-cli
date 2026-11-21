//! The keys that mean the same in every mode, as one table: what they are, when they apply, and
//! what they do.

use color_eyre::eyre::Result;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;

use nori_config::KeyAction;
use nori_config::KeyMode;

use super::super::App;
use super::super::session_view::CARD_PREFIX;
use crate::chatwidget::QuitKeyAction;
use crate::pager_overlay::Overlay;
use crate::subagent_switcher::Target;
use crate::tui::Tui;
use crate::tui::TuiEvent;

pub(super) enum Routed {
    /// The event was dealt with; the bool says whether the app keeps running.
    Handled(bool),
    /// Nobody global wanted it.
    Pass(TuiEvent),
}

/// What a global key does.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Global {
    /// Push the screen into the scrollback and start clean.
    ClearScreen,
    /// Switch to the next or previous subagent view.
    CycleSubagent { reverse: bool },
    /// Go from a child's view to its parent's.
    ParentSession,
    /// Expand or collapse the tool card nearest the composer.
    ToggleTool,
    /// Scroll a child's view by half a screen.
    ScrollChild { up: bool },
}

impl App {
    /// Look the event up in the global table. Quit and the external login take it before anything.
    pub(super) async fn route_global(&mut self, tui: &mut Tui, event: TuiEvent) -> Result<Routed> {
        if let TuiEvent::Key(key) = &event {
            match self.chat_widget.quit_key_action(*key) {
                QuitKeyAction::Exit => {
                    // The canonical journal retains in-flight output; the screen left behind
                    // shows it as it was.
                    self.settle_for_exit();
                    self.chat_widget.begin_immediate_exit();
                    return Ok(Routed::Handled(false));
                }
                QuitKeyAction::Interrupt => {
                    if self.chat_widget.external_cli_login_active() {
                        self.chat_widget.route_external_cli_login_key_event(*key);
                    } else if matches!(self.session_view.focused(), Target::Subagent(_))
                        && !self.chat_widget.has_active_overlay_or_popup()
                    {
                        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
                        self.handle_key(tui, esc).await;
                    } else {
                        self.chat_widget.interrupt_for_quit_key();
                    }
                    return Ok(Routed::Handled(true));
                }
                QuitKeyAction::Continue => {}
            }
        }
        if self.chat_widget.external_cli_login_active() {
            match event {
                TuiEvent::Key(key) => {
                    self.chat_widget.route_external_cli_login_key_event(key);
                    return Ok(Routed::Handled(true));
                }
                TuiEvent::Paste(pasted) => {
                    self.chat_widget.route_external_cli_login_paste(pasted);
                    return Ok(Routed::Handled(true));
                }
                TuiEvent::Draw | TuiEvent::Mouse(_) => {}
            }
        }
        let TuiEvent::Key(key) = &event else {
            return Ok(Routed::Pass(event));
        };
        let Some(action) = self.global_action(key) else {
            return Ok(Routed::Pass(event));
        };
        match action {
            Global::ClearScreen => self.repaint_screen(tui),
            Global::CycleSubagent { reverse } => self.cycle_subagent_session(tui, reverse),
            Global::ParentSession => {
                if let Target::Subagent(child) = self.session_view.focused() {
                    let roster = self.chat_widget.subagent_roster(std::time::Instant::now());
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
            Global::ToggleTool => {
                let nearest = self.chat_widget.nearest_transcript_tool().or_else(|| {
                    tui.last_card(&self.transcript_cells[..self.stage_cell_count()])
                        .map(|cell| format!("{CARD_PREFIX}{cell}"))
                });
                if let Some(call_id) = nearest {
                    self.toggle_tool(tui, call_id);
                } else if self.session_view.focused() == Target::Main {
                    self.open_latest_main_tool(tui);
                }
            }
            Global::ScrollChild { up } => {
                let half = isize::try_from(tui.terminal.size()?.height / 2).unwrap_or(isize::MAX);
                self.session_view.scroll(if up { half } else { -half });
                tui.frame_requester().schedule_frame();
            }
        }
        Ok(Routed::Handled(true))
    }

    /// The global action `key` is, in the state the app is in, if it is one. The keymap says
    /// what a key is; this says whether it applies right now.
    fn global_action(&self, key: &KeyEvent) -> Option<Global> {
        let pressed = matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat);
        // Redraw works everywhere: with a popup open, with a pager overlay open, and while a turn
        // streams. It must not be left to a popup that would swallow the key.
        if key.kind == KeyEventKind::Press
            && crate::nori::hotkey_match::matches_redraw_binding(
                self.hotkey_config
                    .binding_for(nori_config::HotkeyAction::Redraw),
                key,
            )
        {
            return Some(Global::ClearScreen);
        }
        if !pressed {
            return None;
        }
        // A terminal that reports shift+tab as BackTab is asking for shift+tab.
        let mut lookup = *key;
        if lookup.code == KeyCode::BackTab {
            lookup.code = KeyCode::Tab;
            lookup.modifiers |= KeyModifiers::SHIFT;
        }
        let action = self
            .config
            .keymap
            .find(KeyMode::Global, &self.hotkey_config, |binding| {
                crate::nori::hotkey_match::matches_binding(binding, &lookup)
            })?;
        let no_popup = !self.chat_widget.has_active_overlay_or_popup();
        let free_screen = self.overlay.is_none() && no_popup;
        let child_view = matches!(self.session_view.focused(), Target::Subagent(_));
        let press = key.kind == KeyEventKind::Press;
        match action {
            KeyAction::CycleSubagent | KeyAction::CycleSubagentBack
                if no_popup
                    && (self.overlay.is_none()
                        || matches!(self.overlay, Some(Overlay::Subagents(_)))) =>
            {
                Some(Global::CycleSubagent {
                    reverse: action == KeyAction::CycleSubagentBack,
                })
            }
            KeyAction::ParentSession if free_screen && child_view => Some(Global::ParentSession),
            KeyAction::ToggleTool if press && free_screen && self.focus.is_none() => {
                Some(Global::ToggleTool)
            }
            KeyAction::ScrollChildUp | KeyAction::ScrollChildDown
                if press && free_screen && child_view =>
            {
                Some(Global::ScrollChild {
                    up: action == KeyAction::ScrollChildUp,
                })
            }
            // The rest are handled where the composer is (hotkeys) or are not global in
            // this state.
            _ => None,
        }
    }
}
