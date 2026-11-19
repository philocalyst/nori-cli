//! Leaving the screen: what is still in flight joins the conversation, and the shell gets the
//! rows under it.

use std::sync::Arc;

use super::App;
use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::HistoryCell;
use crate::stage::StageView;
use crate::tui::Tui;

impl App {
    /// Move what is still in flight (the streaming answer, the cell being built, tools that were
    /// running) into the conversation, so the screen left behind shows it as it was.
    pub(super) fn settle_for_exit(&mut self) {
        if let Some(cell) = self.chat_widget.take_stream_cell_for_exit() {
            self.transcript_cells.push(cell.into());
        }
        if let Some(cell) = self.chat_widget.take_active_cell_for_exit() {
            self.transcript_cells.push(cell.into());
        }
        for cell in &mut self.transcript_cells {
            if let Some(tool) = cell.as_any().downcast_ref::<ClientToolCell>()
                && tool.is_active()
            {
                let mut failed = tool.clone();
                failed.mark_failed();
                *cell = Arc::new(failed) as Arc<dyn HistoryCell>;
            }
        }
    }

    /// Draw the conversation one last time, without the composer, and hand the terminal back: the
    /// shell's prompt appears under the last row.
    pub(super) fn leave_screen(&mut self, tui: &mut Tui) -> color_eyre::eyre::Result<()> {
        let view = StageView::new(&self.transcript_cells, None);
        tui.draw_stage(&view, 1, 0, |_| {})?;
        tui.finish_screen()?;
        Ok(())
    }
}
