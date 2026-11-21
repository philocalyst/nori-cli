//! Terminal input, routed by mode.
//!
//! Every event takes the same road: the keys that mean the same everywhere (quit, redraw,
//! switching subagents) are looked up in [`global`]'s table; an open overlay owns what is left;
//! otherwise the event goes to the handler of the current [`InputMode`], each of which is a small
//! table of its own (`focus` for a focused prompt, [`keys`] for the composer, [`mouse`] for
//! clicks).

mod global;
mod keys;
mod mouse;

use color_eyre::eyre::Result;
use crossterm::event::KeyEventKind;

use super::App;
use crate::pager_overlay::Overlay;
use crate::tui::Tui;
use crate::tui::TuiEvent;

/// What the keyboard is talking to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InputMode {
    /// A full-screen overlay: the transcript pager, the resume picker, the subagent panel.
    Overlay,
    /// A popup or view of the bottom pane (slash commands, approvals, pickers).
    Panel,
    /// A prompt of the conversation is focused for rewinding.
    Focus,
    /// The composer.
    Composer,
}

impl App {
    pub(super) fn input_mode(&self) -> InputMode {
        if self.overlay.is_some() {
            InputMode::Overlay
        } else if self.chat_widget.has_active_overlay_or_popup() {
            InputMode::Panel
        } else if self
            .focus
            .as_ref()
            .is_some_and(super::focus::Focus::walks_child)
        {
            // Walking back through the main conversation happens in the composer.
            InputMode::Focus
        } else {
            InputMode::Composer
        }
    }

    pub(crate) async fn handle_tui_event(
        &mut self,
        tui: &mut Tui,
        event: TuiEvent,
    ) -> Result<bool> {
        if matches!(&event, TuiEvent::Key(key) if key.kind == KeyEventKind::Press)
            || matches!(&event, TuiEvent::Paste(_))
        {
            self.program_status.acknowledge();
        }
        let event = match self.route_global(tui, event).await? {
            global::Routed::Handled(keep_running) => return Ok(keep_running),
            global::Routed::Pass(event) => event,
        };
        if self.overlay.is_some() {
            tui.set_cursor_style(crossterm::cursor::SetCursorStyle::DefaultUserShape)?;
            if matches!(event, TuiEvent::Draw) {
                tui.set_mouse_capture(true)?;
                // The overlay is a panel under the conversation, which stays on screen above it.
                tui.set_backdrop(&self.transcript_cells[..self.stage_cell_count()]);
            }
            if matches!(self.overlay, Some(Overlay::Subagents(_))) {
                self.handle_subagent_overlay_event(tui, event)?;
            } else {
                self.handle_overlay_event(tui, event)?;
            }
            return Ok(true);
        }
        match event {
            TuiEvent::Draw => return self.draw_frame(tui),
            TuiEvent::Key(key) => self.handle_key(tui, key).await,
            TuiEvent::Paste(text) => self.chat_widget.handle_paste(text),
            TuiEvent::Mouse(mouse) => self.handle_mouse(tui, mouse),
        }
        Ok(true)
    }
}
