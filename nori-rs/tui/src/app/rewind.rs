//! Forking the conversation at a prompt: the one way every rewind goes.
//!
//! The focused prompt's `r`, the edit-and-send of a prompt in place, and the choices of the
//! transcript overlay all end here. The conversation is cut before the prompt, the agent starts a
//! new session that is told what came before, and the prompt is put back for the user (in the
//! composer, or sent at once). The rows of the cut cells that are in the scrollback stay there:
//! the scrollback is history, not state.

use std::any::TypeId;
use std::sync::Arc;

use super::App;
use super::PendingSessionActivation;
use crate::app_event::AgentPrepareIntent;
use crate::app_event::ForkAfter;
use crate::chatwidget::ChatWidget;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::tui::Tui;

impl App {
    /// Cut the conversation before the cell at `cell_index` and carry on from there in a new
    /// session. `after` says what becomes of the prompt that was there.
    pub(super) fn fork_conversation_at(
        &mut self,
        tui: &mut Tui,
        cell_index: usize,
        after: ForkAfter,
    ) {
        let cut = cell_index.min(self.transcript_cells.len());
        let summary = build_fork_summary(&self.transcript_cells, cut);
        let fork_context = (!summary.is_empty()).then_some(summary);
        let draft = self.chat_widget.take_composer_draft();

        self.shutdown_current_conversation();
        let init = self.chat_widget_init(tui.frame_requester(), None, Vec::new(), None, true, None);
        self.chat_widget = ChatWidget::new(init);
        self.configure_new_chat_widget();
        self.pending_session_activation = Some(PendingSessionActivation::New);
        self.begin_agent_preparation_with_context(AgentPrepareIntent::Idle, fork_context);

        self.truncate_transcript(tui, cut);
        self.response_boundaries.retain(|(candidate, _)| {
            self.transcript_cells
                .iter()
                .any(|cell| Arc::ptr_eq(candidate, cell))
        });
        match after {
            ForkAfter::Prefill(text) if !text.is_empty() => {
                self.chat_widget.set_composer_text(text)
            }
            ForkAfter::Prefill(_) => {
                self.chat_widget.restore_composer_draft(draft);
            }
            ForkAfter::Submit(text) => {
                self.chat_widget.restore_composer_draft(draft);
                self.chat_widget.submit_user_prompt_text(text);
            }
        }
        tui.frame_requester().schedule_frame();
    }
}

/// A plain-text account of the conversation before the cell at `cell_index`, which a new session
/// is told so it knows what came before. Every user and assistant message counts, across session
/// boundaries; session markers do not.
///
/// ```text
/// User: <message>
/// Assistant: <text>
/// ```
pub(crate) fn build_fork_summary(cells: &[Arc<dyn HistoryCell>], cell_index: usize) -> String {
    let mut summary = String::new();
    for cell in &cells[..cell_index.min(cells.len())] {
        let any = cell.as_any();
        if let Some(user) = any.downcast_ref::<UserHistoryCell>() {
            summary.push_str(&format!("User: {}\n", user.message));
        } else if any.is::<AgentMessageCell>() || any.is::<AgentMarkdownCell>() {
            let text: String = cell
                .display_lines(u16::MAX)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let text = text.trim();
            if !text.is_empty() {
                summary.push_str(&format!("Assistant: {text}\n"));
            }
        }
    }
    summary
}

/// Every prompt of the conversation, oldest first, as `(cell index, text)`, across session
/// boundaries.
pub(crate) fn collect_all_user_messages(cells: &[Arc<dyn HistoryCell>]) -> Vec<(usize, String)> {
    let user_type = TypeId::of::<UserHistoryCell>();
    cells
        .iter()
        .enumerate()
        .filter(|(_, cell)| cell.as_any().type_id() == user_type)
        .filter_map(|(index, cell)| {
            cell.as_any()
                .downcast_ref::<UserHistoryCell>()
                .map(|user| (index, user.message.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    fn user(text: &str) -> Arc<dyn HistoryCell> {
        Arc::new(UserHistoryCell {
            message: text.to_owned(),
        })
    }

    #[test]
    fn the_summary_holds_what_came_before_the_cut_and_nothing_after() {
        let cells = vec![
            user("hello"),
            Arc::new(AgentMessageCell::new(vec![Line::from("world")], true))
                as Arc<dyn HistoryCell>,
            user("goodbye"),
        ];
        let summary = build_fork_summary(&cells, 2);
        assert!(summary.contains("User: hello"));
        assert!(summary.contains("Assistant:"));
        assert!(!summary.contains("goodbye"));
        assert_eq!(build_fork_summary(&cells, 0), "");
    }

    #[test]
    fn prompts_are_collected_across_the_whole_conversation() {
        let cells = vec![
            user("before"),
            Arc::new(AgentMessageCell::new(vec![Line::from("reply")], true))
                as Arc<dyn HistoryCell>,
            user("after"),
        ];
        assert_eq!(
            collect_all_user_messages(&cells),
            vec![(0, "before".to_owned()), (2, "after".to_owned())]
        );
    }
}
