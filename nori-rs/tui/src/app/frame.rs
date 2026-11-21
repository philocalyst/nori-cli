//! Batching of queued events into a single frame, and drawing it.

use super::*;
use crate::stage::StageView;
use crate::subagent_switcher::Target;

impl App {
    /// Handle a terminal event. A draw first applies every app event that is already queued and
    /// renders the stream once, so any number of events that piled up while the previous frame was
    /// being written cost one frame, not one each.
    pub(super) async fn handle_tui_event_batched(
        &mut self,
        tui: &mut tui::Tui,
        app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
        event: TuiEvent,
    ) -> Result<bool> {
        if !matches!(event, TuiEvent::Draw) {
            return self.handle_tui_event(tui, event).await;
        }
        if !self.apply_queued_events(tui, app_event_rx).await? {
            return Ok(false);
        }
        // Rendering the stream may queue history cells; apply those before drawing as well.
        self.chat_widget.commit_stream_for_frame();
        if !self.apply_queued_events(tui, app_event_rx).await? {
            return Ok(false);
        }
        self.handle_tui_event(tui, event).await
    }

    async fn apply_queued_events(
        &mut self,
        tui: &mut tui::Tui,
        app_event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
    ) -> Result<bool> {
        while let Ok(queued) = app_event_rx.try_recv() {
            // Timed one by one, like the events the loop takes itself (`crate::latency`).
            let kind: &'static str = (&queued).into();
            let started = std::time::Instant::now();
            let keep_running = self.handle_event(tui, queued).await?;
            crate::latency::handled(kind, started.elapsed());
            if !keep_running {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Draw one frame: the conversation on the stage, and under it the bottom block.
    pub(super) fn draw_frame(&mut self, tui: &mut tui::Tui) -> Result<bool> {
        self.validate_focus(tui);
        let projected =
            self.session_view.main_scrolling() || self.session_view.focused() != Target::Main;
        // The conversation is the terminal's text: its wheel scrolls the scrollback and its
        // selection copies, so the mouse is never captured over it. Only a transient panel (a
        // popup, a picker, a child's conversation) takes the mouse, for as long as it is open.
        let _ = tui.set_mouse_capture(
            projected || self.overlay.is_some() || self.chat_widget.has_active_overlay_or_popup(),
        );
        tui.set_cursor_style(self.chat_widget.composer_cursor_style())?;
        self.chat_widget.pre_draw_tick();
        self.update_focused_session_view(tui)?;
        self.chat_widget.maybe_post_pending_notification(tui);
        if self
            .chat_widget
            .handle_paste_burst_tick(tui.frame_requester())
        {
            return Ok(true);
        }
        let screen = tui.terminal.size()?;
        self.chat_widget.set_screen_rows(screen.height);
        let (height, popup_rows) = if projected {
            // A child's conversation, or the main one scrolled back, is a panel under the
            // conversation, never the whole screen.
            let panel = tui::panel_height(screen.height);
            (panel, panel)
        } else {
            let height = self
                .chat_widget
                .desired_height(screen.width)
                .min(screen.height);
            let resting = self.chat_widget.resting_height(screen.width).min(height);
            (height, height - resting)
        };
        let focus = self.focus_view();
        let shown = self.stage_cell_count();
        let view = StageView::new(&self.transcript_cells[..shown], focus.as_ref());
        let chat_widget = &self.chat_widget;
        tui.draw_stage(&view, height, popup_rows, |frame| {
            chat_widget.render(frame.area(), frame.buffer);
            if let Some((x, y)) = chat_widget.cursor_pos(frame.area()) {
                frame.set_cursor_position((x, y));
            }
        })?;
        Ok(true)
    }

    /// How many cells of the conversation the stage shows: a history still being replayed is not
    /// on the stage until it has finished.
    pub(super) fn stage_cell_count(&self) -> usize {
        self.replay_painted_from
            .map_or(self.transcript_cells.len(), |(start, _)| {
                start.min(self.transcript_cells.len())
            })
    }

    /// Clear-screen binding: push everything above the prompt into primary scrollback.
    pub(super) fn repaint_screen(&mut self, tui: &mut tui::Tui) {
        // Remember the transcript boundary so a resize/focus redraw can distinguish the exact
        // clear-screen state from a later turn or a switched/backtracked session.
        self.hidden_transcript_len =
            Some(i64::try_from(self.transcript_cells.len()).unwrap_or(i64::MAX));
        tui.request_clear_screen();
    }
}
