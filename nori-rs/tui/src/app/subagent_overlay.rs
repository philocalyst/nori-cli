//! The subagent switcher in the app: opening it, feeding it what it needs to show, and acting on
//! where the user takes it.

use std::time::Instant;

use nori_config::HotkeyAction;
use ratatui::layout::Rect;

use super::*;
use crate::subagent_switcher::Body;
use crate::subagent_switcher::BodyQuery;
use crate::subagent_switcher::Roster;
use crate::subagent_switcher::SubagentOverlay;
use crate::subagent_switcher::Tail;
use crate::subagent_switcher::Target;
use crate::subagent_switcher::Transition;
use crate::subagent_switcher::cell_lines;

impl App {
    pub(super) fn cycle_subagent_session(&mut self, tui: &mut tui::Tui, reverse: bool) {
        if matches!(self.overlay, Some(Overlay::Subagents(_))) {
            self.close_subagent_switcher(tui);
        }
        let roster = self.subagent_roster();
        let target = self.session_view.step(&roster, reverse, Instant::now());
        self.focus_session_view(tui, target);
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn restore_subagent_draft(
        &mut self,
        tui: &mut tui::Tui,
        key: crate::subagent_switcher::SubagentKey,
        text: &str,
        image_paths: Vec<std::path::PathBuf>,
    ) {
        let target = Target::Subagent(key);
        let current = self.session_view.focused();
        if current != target {
            self.session_view
                .save_draft(current, self.chat_widget.take_composer_draft());
            if let Some(draft) = self.session_view.take_draft(target) {
                self.chat_widget.restore_composer_draft(draft);
            }
        }
        self.chat_widget
            .restore_composer_submission(text.to_owned());
        for path in image_paths {
            let (width, height) = image::image_dimensions(&path).unwrap_or((0, 0));
            self.chat_widget.attach_image(path, width, height, "image");
        }
        if current != target {
            self.session_view
                .save_draft(target, self.chat_widget.take_composer_draft());
            if let Some(draft) = self.session_view.take_draft(current) {
                self.chat_widget.restore_composer_draft(draft);
            }
        }
        tui.frame_requester().schedule_frame();
    }

    /// Open the list of the main session and every subagent.
    pub(super) fn open_actor_panel(&mut self, tui: &mut tui::Tui) {
        let roster = self.subagent_roster();
        let transcript_id = self.active_transcript_id.clone();
        let overlay = if self.subagent_switcher_transcript == transcript_id {
            self.subagent_switcher_state.take().map_or_else(
                || SubagentOverlay::switcher(&roster, self.switcher_binding()),
                |mut overlay| {
                    overlay.update(&roster, Rect::default());
                    *overlay
                },
            )
        } else {
            self.subagent_switcher_state = None;
            self.subagent_switcher_transcript = transcript_id;
            SubagentOverlay::switcher(&roster, self.switcher_binding())
        };
        self.show_subagent_overlay(tui, overlay);
    }

    pub(super) fn focus_subagent_from_mouse(
        &mut self,
        tui: &mut tui::Tui,
        subagent: crate::subagent_switcher::SubagentKey,
    ) {
        self.focus_session_view(tui, Target::Subagent(subagent));
    }

    fn switcher_binding(&self) -> nori_config::HotkeyBinding {
        self.hotkey_config
            .binding_for(HotkeyAction::SwitchSubagent)
            .clone()
    }

    fn show_subagent_overlay(&mut self, tui: &mut tui::Tui, overlay: SubagentOverlay) {
        let _ = tui.set_mouse_capture(true);
        self.overlay = Some(Overlay::Subagents(Box::new(overlay)));
        tui.frame_requester().schedule_frame();
    }

    fn subagent_roster(&self) -> Roster {
        let mut roster = self.chat_widget.subagent_roster(Instant::now());
        // The root conversation's revision is how many cells it has: its preview changes with them.
        roster.main.revision = i64::try_from(self.transcript_cells.len()).unwrap_or(i64::MAX);
        roster
    }

    /// The tail of the root conversation: its last cells, as the transcript overlay shows them.
    fn main_body(&self, query: BodyQuery, revision: i64) -> Body {
        let mut tail = Tail::new(query.rows);
        for cell in self.transcript_cells.iter().rev() {
            if tail.is_full() {
                break;
            }
            tail.push_front(cell_lines(cell.as_ref(), query.width));
        }
        Body {
            query,
            revision,
            lines: tail.finish(),
        }
    }

    /// Bring the open switcher up to date before a frame: a fresh roster, and the conversation
    /// tail it lacks, if any. A frame in which nothing changed costs only the roster.
    fn feed_subagent_overlay(&mut self, tui: &mut tui::Tui) -> color_eyre::eyre::Result<()> {
        let size = tui.terminal.size()?;
        let area = Rect::new(0, 0, size.width, size.height);
        let now = Instant::now();
        let roster = self.subagent_roster();
        let Some(Overlay::Subagents(overlay)) = &mut self.overlay else {
            return Ok(());
        };
        overlay.update(&roster, area);
        let Some(query) = overlay.body_query(area, now) else {
            return Ok(());
        };
        let body = match query.target {
            Target::Main => Some(self.main_body(query, roster.main.revision)),
            Target::Subagent(_) => self.chat_widget.subagent_body(query),
        };
        if let Some(body) = body
            && let Some(Overlay::Subagents(overlay)) = &mut self.overlay
        {
            overlay.set_body(&roster, body, area);
        }
        Ok(())
    }

    /// Route an event to the open switcher or focused view, then carry out where it asked to go.
    pub(super) fn handle_subagent_overlay_event(
        &mut self,
        tui: &mut tui::Tui,
        event: TuiEvent,
    ) -> Result<()> {
        if matches!(event, TuiEvent::Draw) {
            self.chat_widget.subagent_overlay_tick();
            self.feed_subagent_overlay(tui)?;
        }
        let Some(Overlay::Subagents(overlay)) = &mut self.overlay else {
            return Ok(());
        };
        overlay.handle_event(tui, event)?;
        let Some(transition) = overlay.take_transition() else {
            return Ok(());
        };
        match transition {
            Transition::Close => {
                self.close_subagent_switcher(tui);
                tui.frame_requester().schedule_frame();
            }
            Transition::FocusMain => {
                self.close_subagent_switcher(tui);
                self.focus_session_view(tui, Target::Main);
            }
            Transition::Focus(subagent) => {
                self.close_subagent_switcher(tui);
                self.focus_session_view(tui, Target::Subagent(subagent));
            }
            // lens: actions on an actor keep the panel open (see `app/lens.rs`).
            action @ (Transition::Message { .. } | Transition::Cancel(_)) => {
                self.run_actor_action(tui, action);
            }
        }
        Ok(())
    }

    fn close_subagent_switcher(&mut self, tui: &mut tui::Tui) {
        if let Some(Overlay::Subagents(overlay)) = self.overlay.take() {
            self.subagent_switcher_state = Some(overlay);
            self.subagent_switcher_transcript = self.active_transcript_id.clone();
        } else {
            self.overlay = None;
        }
        let _ = tui.set_mouse_capture(false);
        tui.request_full_repaint();
    }
}
