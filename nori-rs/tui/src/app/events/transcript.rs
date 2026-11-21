//! The conversation's cells, rewinds and toasts.

use super::*;

impl App {
    pub(super) async fn handle_transcript_event(
        &mut self,
        tui: &mut tui::Tui,
        event: AppEvent,
    ) -> Result<bool> {
        match event {
            AppEvent::ShowToast(text) => {
                tui.show_toast(text);
            }
            AppEvent::RevealUserQuestion => {
                self.overlay = None;
                self.close_focus(tui);
                tui.request_full_repaint();
                tui.frame_requester().schedule_frame();
            }
            AppEvent::UpdateHistoryToolSnapshot {
                generation,
                snapshot,
            } => {
                if generation == self.chat_widget.session_generation()
                    && let Some(index) =
                        super::tool_history::update(&mut self.transcript_cells, *snapshot)
                {
                    self.invalidate_scrolled_cell(index);
                    tui.frame_requester().schedule_frame();
                }
            }
            AppEvent::FinalizeHistoryTools {
                generation,
                call_ids,
            } => {
                if generation != self.chat_widget.session_generation() {
                    return Ok(true);
                }
                for call_id in call_ids {
                    if let Some(index) =
                        super::tool_history::fail_active(&mut self.transcript_cells, &call_id)
                    {
                        self.invalidate_scrolled_cell(index);
                    }
                }
                tui.frame_requester().schedule_frame();
            }
            AppEvent::InsertHistoryCell(cell) => {
                let cell: Arc<dyn HistoryCell> = cell.into();
                if let Some(Overlay::Transcript(t)) = &mut self.overlay {
                    t.insert_cell(cell.clone());
                    tui.frame_requester().schedule_frame();
                }
                self.transcript_cells.push(cell);
                tui.frame_requester().schedule_frame();
            }
            AppEvent::ResponseRewindFinished {
                generation,
                cell,
                result,
            } => {
                if self.chat_widget.session_generation() == generation {
                    match result {
                        Ok(()) => {
                            if let Some(index) = self
                                .transcript_cells
                                .iter()
                                .position(|candidate| Arc::ptr_eq(candidate, &cell))
                            {
                                self.transcript_cells.truncate(index + 1);
                                self.response_boundaries.retain(|(candidate, _)| {
                                    self.transcript_cells
                                        .iter()
                                        .any(|cell| Arc::ptr_eq(candidate, cell))
                                });
                                self.hidden_transcript_len = None;
                                // Existing primary-screen scrollback is terminal history. Keep it
                                // intact; subsequent responses continue from the rewound head.
                                tui.frame_requester().schedule_frame();
                            }
                        }
                        Err(error) => self
                            .chat_widget
                            .add_error_message(format!("Could not rewind conversation: {error}")),
                    }
                }
            }
            AppEvent::FocusSnapshotsLoaded {
                generation,
                snapshots,
            } => {
                self.apply_focus_snapshots(generation, snapshots);
                tui.frame_requester().schedule_frame();
            }
            // The composer's walk back through the prompts. Interim: the focus presentation
            // and the rewind itself are being rebuilt around these events (app/focus.rs).
            AppEvent::FocusChanged { generation, cell } => {
                self.on_focus_changed(tui, generation, cell);
            }
            AppEvent::ForkWalkedTurn {
                generation,
                cell_index,
            } => self.fork_walked_turn(generation, cell_index),
            AppEvent::RewindSelection(selection) => {
                self.on_rewind_selection(tui, selection);
            }
            AppEvent::RewindFilesRestored {
                generation,
                cell_index,
                after,
                result,
            } => self.on_rewind_files_restored(tui, generation, cell_index, after, result),
            AppEvent::ResponseBoundary(boundary) => {
                if let Some(cell) = self.transcript_cells.iter().rev().find(|cell| {
                    (cell.as_any().is::<crate::history_cell::AgentMessageCell>()
                        && !cell.is_stream_continuation())
                        || cell.as_any().is::<crate::history_cell::AgentMarkdownCell>()
                }) {
                    self.response_boundaries.push((cell.clone(), boundary));
                }
            }
            AppEvent::HistoryReplay(crate::app_event::HistoryReplay::Started(source)) => {
                self.replay_painted_from = Some((self.transcript_cells.len(), source));
            }
            AppEvent::HistoryReplay(crate::app_event::HistoryReplay::Finished) => {
                self.paint_replayed_history(tui);
            }
            AppEvent::ConsolidateAgentMessage { source, cwd } => {
                self.consolidate_agent_message(tui, source, &cwd);
            }
            AppEvent::DiffResult(changes) => {
                self.chat_widget.on_diff_complete();
                // The changes are drawn like any edit, at whatever width the pager has.
                let renderables: Vec<Box<dyn crate::render::renderable::Renderable>> = match changes
                {
                    crate::get_git_diff::GitChanges::NotARepo => {
                        vec![Box::new("Not inside a git repository.".italic())]
                    }
                    crate::get_git_diff::GitChanges::Failed(error) => {
                        vec![Box::new(format!("Couldn't read the changes: {error}"))]
                    }
                    crate::get_git_diff::GitChanges::Changes { files, binary } => {
                        let mut renderables: Vec<Box<dyn crate::render::renderable::Renderable>> =
                            Vec::new();
                        if files.is_empty() && binary.is_empty() {
                            renderables.push(Box::new("No changes.".italic()));
                        }
                        if !files.is_empty() {
                            let cwd = self.chat_widget.config_ref().cwd.clone();
                            renderables.push(Box::new(DiffSummary::new(
                                files,
                                cwd,
                                crate::diff_render::ChangesShown::WorkingTree,
                            )));
                        }
                        if !binary.is_empty() {
                            renderables
                                .push(Box::new(format!("Binary: {}", binary.join(", ")).dim()));
                        }
                        renderables
                    }
                };
                self.overlay = Some(Overlay::new_static_with_renderables(
                    renderables,
                    "D I F F".to_string(),
                ));
                tui.frame_requester().schedule_frame();
            }
            AppEvent::TransientNotice(message) => tui.show_toast(message),
            AppEvent::ForkToMessage {
                cell_index,
                prefill,
            } => self.fork_conversation_at(
                tui,
                cell_index,
                crate::app_event::ForkAfter::Prefill(prefill),
            ),
            // Ctrl+Space: the actor panel (`subagent_switcher`).
            AppEvent::ShellComplete { text, cursor } => {
                self.chat_widget.request_shell_completion(text, cursor);
            }
            AppEvent::ShellCompleted { text, candidates } => {
                self.chat_widget.apply_shell_completion(&text, &candidates);
                tui.frame_requester().schedule_frame();
            }
            other => unreachable!("routed to the wrong domain: {other:?}"),
        }
        Ok(true)
    }
}
