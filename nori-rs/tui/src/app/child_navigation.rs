//! Response navigation and journal actions for a focused child conversation.

use std::sync::Arc;

use crate::app_event::AppEvent;
use crate::history_cell::HistoryCell;
use crate::pager_overlay::MessageAction;
use crate::pager_overlay::MessageChoice;
use crate::pager_overlay::Overlay;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Target;
use crate::tui;

pub(crate) struct PendingChildFocus {
    pub(crate) key: SubagentKey,
    pub(crate) session_id: nori_protocol::acp::v1::SessionId,
    pub(crate) generation: crate::app_event::SessionGeneration,
    pub(crate) request_id: i64,
    pub(crate) keys: Vec<crossterm::event::KeyEvent>,
}

pub(super) struct ChildHistory {
    key: SubagentKey,
    cells: Vec<Arc<dyn HistoryCell>>,
    boundaries: Vec<(usize, nori_protocol::ConversationResponseBoundary)>,
}

impl super::App {
    pub(super) fn show_child_history_error(
        &mut self,
        tui: &mut tui::Tui,
        key: SubagentKey,
        error: String,
    ) {
        if self.session_view.focused() == Target::Subagent(key) {
            self.session_view.append_child_notice(key, &error);
        } else {
            self.chat_widget.add_error_message(error);
        }
        tui.frame_requester().schedule_frame();
    }
    pub(super) fn request_child_history(
        &mut self,
        tui: &mut tui::Tui,
        key: SubagentKey,
        focus_responses: bool,
    ) {
        let Some(parent) = self.chat_widget.harness_handle() else {
            self.show_child_history_error(
                tui,
                key,
                "This child does not expose a conversation journal.".to_owned(),
            );
            return;
        };
        let Some(session_id) = self.chat_widget.subagent_session(key) else {
            self.show_child_history_error(
                tui,
                key,
                "This child has no session available for history actions.".to_owned(),
            );
            return;
        };
        self.child_focus_sequence += 1;
        let request_id = self.child_focus_sequence;
        if focus_responses {
            self.pending_child_focus = Some(PendingChildFocus {
                key,
                session_id: session_id.clone(),
                generation: self.chat_widget.session_generation(),
                request_id,
                keys: Vec::new(),
            });
        } else {
            self.pending_child_focus = None;
        }
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = parent
                .child_history(session_id)
                .await
                .map_err(|error| error.to_string());
            tx.send(AppEvent::ChildHistoryReady {
                request_id,
                key,
                focus_responses,
                result,
            });
        });
    }

    pub(super) fn show_child_history(
        &mut self,
        tui: &mut tui::Tui,
        key: SubagentKey,
        focus_responses: bool,
        events: Vec<nori_protocol::SessionEvent>,
    ) {
        if self.session_view.focused() != Target::Subagent(key) || self.overlay.is_some() {
            return;
        }
        let (cells, boundaries) =
            crate::chatwidget::subagents::project_child_history(&events, &self.config.cwd);
        self.session_view.child_history = Some(ChildHistory {
            key,
            cells: cells.clone(),
            boundaries,
        });
        if focus_responses {
            self.begin_focus(tui, cells, false);
            return;
        }
        let _ = tui.set_mouse_capture(true);
        self.overlay = Some(Overlay::new_transcript(cells));
        tui.frame_requester().schedule_frame();
    }

    pub(super) fn on_child_message_action(&mut self, tui: &mut tui::Tui, action: MessageAction) {
        let Some(history) = self.session_view.child_history.take() else {
            return;
        };
        self.close_transcript_overlay(tui);
        let source = history
            .boundaries
            .iter()
            .rev()
            .find(|(index, _)| *index == action.cell)
            .map(|(_, boundary)| (boundary.transcript_id.clone(), boundary.start, boundary.end));
        let Some(source) = source else {
            self.show_child_history_error(
                tui,
                history.key,
                "This message has no recorded response source.".to_owned(),
            );
            return;
        };
        let rewound = match action.choice {
            MessageChoice::RollbackThrough => true,
            MessageChoice::ForkSubagent | MessageChoice::ForkThrough => false,
            MessageChoice::EditPrompt | MessageChoice::Rewind => {
                self.show_child_history_error(
                    tui,
                    history.key,
                    "This child history action is unavailable.".to_owned(),
                );
                return;
            }
        };
        let Some(parent) = self.chat_widget.harness_handle() else {
            return;
        };
        let Some(session_id) = self.chat_widget.subagent_session(history.key) else {
            return;
        };
        let key = history.key;
        let context = history
            .cells
            .iter()
            .take(action.cell + 1)
            .flat_map(|cell| cell.display_lines(u16::MAX))
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = async {
                if rewound {
                    parent
                        .rewind_child_history(session_id.clone(), source)
                        .await?;
                } else {
                    parent
                        .fork_child_history(session_id.clone(), context, source)
                        .await?;
                }
                parent.child_history(session_id).await
            }
            .await
            .map_err(|error| error.to_string());
            tx.send(AppEvent::ChildHistoryActionFinished {
                key,
                rewound,
                result,
            });
        });
    }
}
