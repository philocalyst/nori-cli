//! Carrying out what the user chose for a message of the transcript overlay.

use std::sync::Arc;

use super::App;
use crate::app_event::AppEvent;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::pager_overlay::MessageAction;
use crate::pager_overlay::MessageChoice;
use crate::tui;

impl App {
    pub(crate) fn on_message_action(&mut self, tui: &mut tui::Tui, action: MessageAction) {
        if self.session_view.focused() != crate::subagent_switcher::Target::Main {
            self.on_child_message_action(tui, action);
            return;
        }
        self.close_transcript_overlay(tui);
        tui.frame_requester().schedule_frame();
        if self.chat_widget.is_task_running() && action.choice != MessageChoice::ForkSubagent {
            self.chat_widget.add_info_message(
                "A turn is running: stop it with Esc before forking or rewinding.".to_owned(),
                None,
            );
            return;
        }
        let prompt_at = |cell: usize| {
            self.transcript_cells
                .get(cell)
                .and_then(|cell| cell.as_any().downcast_ref::<UserHistoryCell>())
                .map(|user| user.message.clone())
        };
        match action.choice {
            MessageChoice::EditPrompt => {
                let prefill = prompt_at(action.cell).unwrap_or_default();
                self.app_event_tx.send(AppEvent::ForkToMessage {
                    cell_index: action.cell,
                    prefill,
                });
            }
            MessageChoice::ForkThrough => {
                let cut = end_of_message(&self.transcript_cells, action.cell);
                // Nothing said after it: the agent's own fork keeps everything, tool calls
                // included, where a fork before a message has only a summary to go on.
                let at_head = self.transcript_cells[cut..].iter().all(|cell| {
                    !cell.as_any().is::<UserHistoryCell>()
                        && !cell.as_any().is::<AgentMessageCell>()
                        && !cell.as_any().is::<AgentMarkdownCell>()
                });
                if at_head && self.chat_widget.agent_capabilities().session_fork {
                    self.app_event_tx.send(AppEvent::BranchFromCurrent);
                } else {
                    self.app_event_tx.send(AppEvent::ForkToMessage {
                        cell_index: cut,
                        prefill: String::new(),
                    });
                }
            }
            MessageChoice::RollbackThrough => {
                let cell = self.transcript_cells[action.cell].clone();
                let source = response_source(&self.response_boundaries, &cell);
                if let Some((transcript_id, _, end)) = source
                    && self.active_transcript_id.as_deref() == Some(transcript_id.as_str())
                    && let Some(handle) = self.chat_widget.harness_handle()
                {
                    let tx = self.app_event_tx.clone();
                    let generation = self.chat_widget.session_generation();
                    tokio::spawn(async move {
                        let result = handle
                            .rewind_and_rebind(transcript_id, Some(end))
                            .await
                            .map_err(|error| error.to_string());
                        tx.send(AppEvent::ResponseRewindFinished {
                            generation,
                            cell,
                            result,
                        });
                    });
                } else {
                    self.chat_widget.add_error_message(
                        "This response has no recorded source for rollback.".to_owned(),
                    );
                }
            }
            MessageChoice::ForkSubagent => self.fork_subagent_from(action.cell),
            MessageChoice::Rewind => {
                let prompts = (action.cell..self.transcript_cells.len())
                    .filter_map(prompt_at)
                    .collect();
                let prompt = prompt_at(action.cell).unwrap_or_default();
                self.chat_widget.begin_rewind(crate::chatwidget::Rewind {
                    cell_index: action.cell,
                    prompt,
                    prompts,
                });
            }
        }
    }
}

impl App {
    /// A subagent goes on from the response at `cell`: it is given the conversation up to the
    /// end of that response.
    pub(super) fn fork_subagent_from(&mut self, cell: usize) {
        let cut = end_of_message(&self.transcript_cells, cell);
        let context = self.transcript_cells[..cut]
            .iter()
            .flat_map(|cell| cell.display_lines(u16::MAX))
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let source = self
            .transcript_cells
            .get(cell)
            .and_then(|cell| response_source(&self.response_boundaries, cell));
        self.app_event_tx
            .send(AppEvent::ForkResponseSubagent { context, source });
    }
}

fn response_source(
    boundaries: &[(
        Arc<dyn HistoryCell>,
        nori_protocol::ConversationResponseBoundary,
    )],
    cell: &Arc<dyn HistoryCell>,
) -> Option<(String, i64, i64)> {
    boundaries.iter().rev().find_map(|(candidate, boundary)| {
        Arc::ptr_eq(candidate, cell)
            .then(|| (boundary.transcript_id.clone(), boundary.start, boundary.end))
    })
}

/// Where the message at `cell` ends: after it and the cells that continue it.
fn end_of_message(cells: &[Arc<dyn HistoryCell>], cell: usize) -> usize {
    let mut end = cell + 1;
    while cells
        .get(end)
        .is_some_and(|next| next.is_stream_continuation())
    {
        end += 1;
    }
    end.min(cells.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    #[test]
    fn source_is_bound_to_the_rendered_cell_not_matching_text() {
        let cell: Arc<dyn HistoryCell> =
            Arc::new(AgentMessageCell::new(vec![Line::from("same")], true));
        let duplicate: Arc<dyn HistoryCell> =
            Arc::new(AgentMessageCell::new(vec![Line::from("same")], true));
        let boundaries = vec![(
            cell.clone(),
            nori_protocol::ConversationResponseBoundary {
                transcript_id: "session".into(),
                start: 3,
                end: 9,
            },
        )];
        assert_eq!(
            response_source(&boundaries, &cell),
            Some(("session".into(), 3, 9))
        );
        assert_eq!(response_source(&boundaries, &duplicate), None);
    }

    #[test]
    fn a_consolidated_response_keeps_its_source_for_rollback_and_fork() {
        let first: Arc<dyn HistoryCell> =
            Arc::new(AgentMessageCell::new(vec![Line::from("first")], true));
        let continuation: Arc<dyn HistoryCell> =
            Arc::new(AgentMessageCell::new(vec![Line::from("final")], false));
        let mut cells = vec![first, continuation.clone()];

        let (_, consolidated) = crate::transcript_cells::consolidate_agent_message_cells(
            &mut cells,
            "first\nfinal".to_owned(),
            std::path::Path::new("/work"),
        )
        .expect("the streamed answer is consolidated");
        let boundary = nori_protocol::ConversationResponseBoundary {
            transcript_id: "session".to_owned(),
            start: 3,
            end: 9,
        };
        // App event ordering consolidates the stream before applying its response boundary.
        // Both rollback and fork resolve source through this same cell-identity lookup.
        let boundaries = vec![(consolidated.clone(), boundary)];

        assert!(consolidated.as_any().is::<AgentMarkdownCell>());
        assert_eq!(
            response_source(&boundaries, &consolidated),
            Some(("session".to_owned(), 3, 9))
        );
        assert_eq!(response_source(&boundaries, &continuation), None);
    }

    #[test]
    fn a_message_ends_after_the_cells_that_continue_it() {
        let cells: Vec<Arc<dyn HistoryCell>> = vec![
            Arc::new(UserHistoryCell {
                message: "question".to_owned(),
            }),
            Arc::new(AgentMessageCell::new(vec![Line::from("first")], true)),
            Arc::new(AgentMessageCell::new(vec![Line::from("more")], false)),
            Arc::new(UserHistoryCell {
                message: "next".to_owned(),
            }),
        ];
        assert_eq!(end_of_message(&cells, 1), 3);
        assert_eq!(end_of_message(&cells, 0), 1);
        assert_eq!(end_of_message(&cells, 3), 4);
    }
}
