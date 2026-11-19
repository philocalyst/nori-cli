//! Edits to the conversation's cells that the screen must hear about: the stage addresses cells
//! by their place in the list, so removing or merging cells moves what it has already committed.

use std::path::Path;

use super::App;
use crate::pager_overlay::Overlay;
use crate::tui::Tui;

impl App {
    /// Drop every cell from `len` on (a rewind). What of them is in the scrollback stays there.
    pub(super) fn truncate_transcript(&mut self, tui: &mut Tui, len: usize) {
        self.transcript_cells.truncate(len);
        self.hidden_transcript_len = None;
        tui.stage_truncated(len);
        tui.frame_requester().schedule_frame();
    }

    /// Replace the streamed pieces of the last answer with one cell made from its full source.
    pub(super) fn consolidate_agent_message(&mut self, tui: &mut Tui, source: String, cwd: &Path) {
        let Some((range, replacement)) = crate::transcript_cells::consolidate_agent_message_cells(
            &mut self.transcript_cells,
            source,
            cwd,
        ) else {
            return;
        };
        tui.stage_replaced(range.clone(), 1);
        if let Some(Overlay::Transcript(transcript)) = &mut self.overlay {
            transcript.replace_cells(range, replacement);
        }
        tui.frame_requester().schedule_frame();
    }

    /// A cell the in-app scrolling view may have cached changed.
    pub(super) fn invalidate_scrolled_cell(&mut self, index: usize) {
        let hidden = self
            .hidden_transcript_len
            .and_then(|length| usize::try_from(length).ok())
            .unwrap_or(0);
        if let Some(visible) = index.checked_sub(hidden) {
            self.session_view.invalidate_main_transcript_cell(visible);
        }
    }
}
