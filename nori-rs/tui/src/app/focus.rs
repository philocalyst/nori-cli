//! Walking back to an earlier prompt, and rewinding or forking from it.
//!
//! The main conversation is walked back from the composer itself: ↑ at its top loads the previous
//! prompt into the composer's editor, and the composer tells the app where it is
//! (`AppEvent::FocusChanged`). The focus follows: the stage draws that prompt edited in place,
//! with the editor's rows and cursor, and everything after it dimmed (see `stage::focus`), so
//! what sending from there would roll back is visible; the composer has no text box meanwhile. Enter in the composer (`AppEvent::RewindSelection`)
//! restores the files of the turns it rolls back (Alt+Enter keeps them), forks the conversation
//! before the prompt and sends the edited text. A running turn is stopped first, never waited
//! for by the person: the rewind happens as soon as it has stopped.
//!
//! A child's journal is walked with keys, its conversation drawn by the child's view; its only
//! actions are the journal's (rollback and fork):
//!
//! | key | what |
//! |---|---|
//! | `[` `]`, `↑` `↓` | move between prompts |
//! | `Enter`, `e`, `r` | roll back through it |
//! | `f`, `Space` | fork a subagent from its response |
//! | `Esc` | leave |

use std::sync::Arc;

use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use nori_config::KeyAction;
use nori_config::KeyMode;
use nori_harness::UndoSnapshot;

use super::App;
use crate::app_event::AppEvent;
use crate::app_event::ForkAfter;
use crate::app_event::RewindFiles;
use crate::app_event::RewindSelection;
use crate::app_event::SessionGeneration;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::pager_overlay::MessageAction;
use crate::pager_overlay::MessageChoice;
use crate::stage::FocusView;
use crate::subagent_switcher::Target;
use crate::tui::Tui;

/// The prompt a rewind is focused on.
pub(crate) struct Focus {
    target: Target,
    /// A child's journal, when the focus walks one; the main conversation is read from the app.
    child_cells: Vec<Arc<dyn HistoryCell>>,
    /// Index of the focused prompt in the cells walked.
    selected: usize,
    /// What the edit journal says restoring would undo, once it answered.
    snapshots: Option<Vec<UndoSnapshot>>,
    /// Opened with `[`: a `t` right after toggles the nearest tool card instead.
    opened_by_bracket: bool,
}

impl Focus {
    /// The focus walks a child's journal, with keys; the main conversation is walked from the
    /// composer.
    pub(crate) fn walks_child(&self) -> bool {
        self.target != Target::Main
    }
}

pub(crate) fn is_prompt(cell: &Arc<dyn HistoryCell>) -> bool {
    cell.as_any().is::<UserHistoryCell>()
}

fn is_response(cell: &Arc<dyn HistoryCell>) -> bool {
    (cell.as_any().is::<AgentMessageCell>() && !cell.is_stream_continuation())
        || cell.as_any().is::<AgentMarkdownCell>()
}

/// The last response of the turn that begins with the prompt at `prompt`.
fn response_for_prompt(cells: &[Arc<dyn HistoryCell>], prompt: usize) -> Option<usize> {
    let tail = cells.get(prompt.checked_add(1)?..)?;
    let end = tail.iter().position(is_prompt).unwrap_or(tail.len());
    tail[..end]
        .iter()
        .rposition(is_response)
        .map(|index| prompt + 1 + index)
}

/// Which prompt `selected` is, and how many there are.
fn turn_position(cells: &[Arc<dyn HistoryCell>], selected: usize) -> (usize, usize) {
    let total = cells.iter().filter(|cell| is_prompt(cell)).count();
    let turn = cells[..=selected.min(cells.len().saturating_sub(1))]
        .iter()
        .filter(|cell| is_prompt(cell))
        .count();
    (turn.max(1), total.max(1))
}

fn adjacent_prompt(cells: &[Arc<dyn HistoryCell>], current: usize, older: bool) -> Option<usize> {
    if older {
        cells[..current].iter().rposition(is_prompt)
    } else {
        cells
            .iter()
            .enumerate()
            .skip(current + 1)
            .find_map(|(index, cell)| is_prompt(cell).then_some(index))
    }
}

impl App {
    /// The cells the focus walks.
    fn focus_cells(&self) -> &[Arc<dyn HistoryCell>] {
        match &self.focus {
            Some(focus) if focus.target != Target::Main => &focus.child_cells,
            Some(_) | None => &self.transcript_cells,
        }
    }

    /// Focus the last prompt of a child's journal, whose cells are given.
    pub(super) fn begin_focus(
        &mut self,
        tui: &mut Tui,
        child_cells: Vec<Arc<dyn HistoryCell>>,
        opened_by_bracket: bool,
    ) {
        let target = self.session_view.focused();
        let Some(selected) = child_cells.iter().rposition(is_prompt) else {
            self.chat_widget
                .add_info_message("Nothing has been sent yet to rewind to.".to_owned(), None);
            return;
        };
        self.focus = Some(Focus {
            target,
            child_cells,
            selected,
            snapshots: None,
            opened_by_bracket,
        });
        tui.frame_requester().schedule_frame();
    }

    /// The composer walked back to the prompt at `cell` of the main conversation, or left the
    /// walk (`None`).
    pub(super) fn on_focus_changed(
        &mut self,
        tui: &mut Tui,
        generation: SessionGeneration,
        cell: Option<usize>,
    ) {
        if generation != self.chat_widget.session_generation()
            || self.session_view.focused() != Target::Main
        {
            return;
        }
        let selected = cell.filter(|cell| self.transcript_cells.get(*cell).is_some_and(is_prompt));
        match (selected, self.focus.as_mut()) {
            (Some(selected), Some(focus)) if focus.target == Target::Main => {
                focus.selected = selected;
            }
            (Some(selected), _) => {
                self.focus = Some(Focus {
                    target: Target::Main,
                    child_cells: Vec::new(),
                    selected,
                    snapshots: None,
                    opened_by_bracket: false,
                });
                // What restoring would undo, for the card's header; it arrives when it arrives.
                if let Some(handle) = self.chat_widget.harness_handle() {
                    let tx = self.app_event_tx.clone();
                    tokio::spawn(async move {
                        if let Ok(snapshots) = handle.undo_snapshots().await {
                            tx.send(AppEvent::FocusSnapshotsLoaded {
                                generation,
                                snapshots,
                            });
                        }
                    });
                }
            }
            (None, Some(focus)) if focus.target == Target::Main => self.focus = None,
            (None, _) => {}
        }
        tui.frame_requester().schedule_frame();
    }

    /// Enter on a prompt the composer walked back to: rewind there and send the edited text. A
    /// running turn is stopped first and the rewind follows as soon as it has stopped.
    pub(super) fn on_rewind_selection(&mut self, tui: &mut Tui, selection: RewindSelection) {
        if selection.generation != self.chat_widget.session_generation() {
            return;
        }
        let still_there = self
            .transcript_cells
            .get(selection.cell_index)
            .is_some_and(is_prompt)
            && turn_position(&self.transcript_cells, selection.cell_index).0 == selection.turn;
        if !still_there {
            self.focus = None;
            self.chat_widget.add_error_message(
                "That prompt is no longer in the conversation; nothing was rewound.".to_owned(),
            );
            return;
        }
        if self.chat_widget.is_task_running() {
            self.chat_widget
                .submit_harness_action(crate::app_event::HarnessAction::Cancel);
            self.chat_widget.add_info_message(
                "Stopping the running turn; the rewind follows as soon as it has stopped."
                    .to_owned(),
                None,
            );
            self.pending_rewind = Some(selection);
            return;
        }
        self.rewind_to(tui, selection);
    }

    fn rewind_to(&mut self, tui: &mut Tui, selection: RewindSelection) {
        let snapshots = self.focus.take().and_then(|focus| focus.snapshots);
        self.focus = Some(Focus {
            target: Target::Main,
            child_cells: Vec::new(),
            selected: selection.cell_index,
            snapshots,
            opened_by_bracket: false,
        });
        self.dispatch_rewind(
            tui,
            selection.files == RewindFiles::Restore,
            ForkAfter::Submit(selection.text),
        );
    }

    /// Ctrl+F on a walked prompt: a subagent goes on from the end of that turn's answer.
    pub(super) fn fork_walked_turn(&mut self, generation: SessionGeneration, cell_index: usize) {
        if generation != self.chat_widget.session_generation() {
            return;
        }
        match response_for_prompt(&self.transcript_cells, cell_index) {
            Some(response) => self.fork_subagent_from(response),
            None => self.chat_widget.add_info_message(
                "That turn has no answer yet to go on from.".to_owned(),
                None,
            ),
        }
    }

    pub(super) fn apply_focus_snapshots(
        &mut self,
        generation: SessionGeneration,
        snapshots: Vec<UndoSnapshot>,
    ) {
        if self.chat_widget.session_generation() == generation
            && let Some(focus) = self.focus.as_mut()
        {
            focus.snapshots = Some(snapshots);
        }
    }

    pub(super) fn close_focus(&mut self, tui: &mut Tui) {
        self.focus = None;
        tui.frame_requester().schedule_frame();
    }

    /// The focused prompt as the stage draws it, for the main conversation `width` wide: edited
    /// in place, with the composer's editor.
    pub(crate) fn focus_view(&self, width: u16) -> Option<FocusView> {
        let focus = self
            .focus
            .as_ref()
            .filter(|focus| focus.target == Target::Main)?;
        Some(FocusView {
            selected: focus.selected,
            edit: self.chat_widget.inline_walk(width),
        })
    }

    /// A child's journal as drawn while one of its prompts is focused.
    pub(crate) fn focus_child_rows(&self, width: u16) -> Option<crate::stage::FocusedColumn> {
        let focus = self
            .focus
            .as_ref()
            .filter(|focus| focus.target != Target::Main)?;
        Some(crate::stage::render_focused(
            &FocusView {
                selected: focus.selected,
                edit: None,
            },
            &focus.child_cells,
            width,
        ))
    }

    /// The text of the prompts from the one at `from` to the newest, oldest first.
    fn prompts_from(&self, from: usize) -> Vec<String> {
        self.focus_cells()[from.min(self.focus_cells().len())..]
            .iter()
            .filter_map(|cell| {
                cell.as_any()
                    .downcast_ref::<UserHistoryCell>()
                    .map(|user| user.message.clone())
            })
            .collect()
    }

    /// Keep the focus honest: its prompt must still be one. A rewind waiting for a turn to stop
    /// goes ahead once it has.
    pub(super) fn validate_focus(&mut self, tui: &mut Tui) {
        if let Some(selection) = self
            .pending_rewind
            .take_if(|_| !self.chat_widget.is_task_running())
        {
            self.rewind_to(tui, selection);
            return;
        }
        let valid = self.focus.as_ref().is_none_or(|focus| {
            focus.target == self.session_view.focused()
                && self
                    .focus_cells()
                    .get(focus.selected)
                    .is_some_and(is_prompt)
        });
        if !valid {
            self.close_focus(tui);
        }
    }

    /// Handle a key while a prompt is focused. Returns whether the key was the focus's.
    pub(super) fn handle_focus_key(&mut self, tui: &mut Tui, key: KeyEvent) -> bool {
        if self.focus.is_none() {
            return false;
        }
        if key.kind == KeyEventKind::Release {
            return true;
        }
        let opened_by_bracket = self
            .focus
            .as_mut()
            .is_some_and(|focus| std::mem::take(&mut focus.opened_by_bracket));
        let Some(action) =
            self.config
                .keymap
                .find(KeyMode::Focus, &self.hotkey_config, |binding| {
                    crate::nori::hotkey_match::matches_binding(binding, &key)
                })
        else {
            // Every other key is the focus's too: nothing is typed into the composer meanwhile.
            return true;
        };
        match action {
            KeyAction::FocusOlder | KeyAction::FocusNewer => {
                self.move_focus(tui, action == KeyAction::FocusOlder);
            }
            KeyAction::FocusEdit | KeyAction::FocusRewind => self.rewind_from_focus(tui, true),
            KeyAction::FocusRewindKeepFiles => self.rewind_from_focus(tui, false),
            KeyAction::FocusFork => self.fork_from_focus(tui),
            KeyAction::FocusToggleTool if opened_by_bracket => {
                self.close_focus(tui);
                if let Some(call_id) = self.chat_widget.nearest_transcript_tool() {
                    self.toggle_tool(tui, call_id);
                } else if self.session_view.focused() == Target::Main {
                    self.open_latest_main_tool(tui);
                }
            }
            KeyAction::FocusToggleTool => {}
            KeyAction::FocusLeave => self.close_focus(tui),
            // Entries of other modes are not looked up here.
            _ => {}
        }
        true
    }

    fn move_focus(&mut self, tui: &mut Tui, older: bool) {
        let Some(selected) = self.focus.as_ref().map(|focus| focus.selected) else {
            return;
        };
        match adjacent_prompt(self.focus_cells(), selected, older) {
            Some(next) => {
                if let Some(focus) = self.focus.as_mut() {
                    focus.selected = next;
                }
                tui.frame_requester().schedule_frame();
            }
            // Past the newest prompt is the composer again.
            None if !older => self.close_focus(tui),
            None => {}
        }
    }

    fn rewind_from_focus(&mut self, tui: &mut Tui, restore_files: bool) {
        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        if focus.target != Target::Main {
            self.child_message_action(tui, MessageChoice::RollbackThrough);
            return;
        }
        let prompt = self
            .prompts_from(focus.selected)
            .into_iter()
            .next()
            .unwrap_or_default();
        self.dispatch_rewind(tui, restore_files, ForkAfter::Prefill(prompt));
    }

    fn fork_from_focus(&mut self, tui: &mut Tui) {
        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        if focus.target != Target::Main {
            self.child_message_action(tui, MessageChoice::ForkSubagent);
            return;
        }
        let Some(response) = response_for_prompt(&self.transcript_cells, focus.selected) else {
            self.chat_widget.add_info_message(
                "This prompt has no completed response to fork from.".to_owned(),
                None,
            );
            return;
        };
        self.close_focus(tui);
        self.on_message_action(
            tui,
            MessageAction {
                cell: response,
                choice: MessageChoice::ForkSubagent,
            },
        );
    }

    /// Carry out a journal action on the response of the focused prompt of a child.
    fn child_message_action(&mut self, tui: &mut Tui, choice: MessageChoice) {
        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        let Some(response) = response_for_prompt(&focus.child_cells, focus.selected) else {
            self.chat_widget.add_info_message(
                "This prompt has no completed response to select.".to_owned(),
                None,
            );
            return;
        };
        self.close_focus(tui);
        self.on_message_action(
            tui,
            MessageAction {
                cell: response,
                choice,
            },
        );
    }

    /// Rewind the main conversation to the focused prompt. With `restore_files`, the files the
    /// turns from there on changed are restored first, from the edit journal.
    fn dispatch_rewind(&mut self, tui: &mut Tui, restore_files: bool, after: ForkAfter) {
        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        let cell_index = focus.selected;
        let prompts = self.prompts_from(cell_index);
        let cached = focus.snapshots.clone();
        self.close_focus(tui);
        let generation = self.chat_widget.session_generation();
        let handle = self.chat_widget.harness_handle().filter(|_| restore_files);
        let Some(handle) = handle else {
            self.fork_conversation_at(tui, cell_index, after);
            return;
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let snapshots = match cached {
                Some(snapshots) if !snapshots.is_empty() => Ok(snapshots),
                _ => handle
                    .undo_snapshots()
                    .await
                    .map_err(|error| error.to_string()),
            };
            let result = match snapshots {
                Ok(snapshots) => {
                    let undone = crate::chatwidget::rewind::turns_to_undo(&snapshots, &prompts);
                    match undone.checked_sub(1) {
                        Some(last) => {
                            handle
                                .undo_to(snapshots[last].index)
                                .await
                                .map_err(|error| {
                                    format!("Rewind could not restore the files: {error:#}")
                                })
                        }
                        None => Ok(()),
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "undo snapshots unavailable during rewind");
                    Ok(())
                }
            };
            tx.send(AppEvent::RewindFilesRestored {
                generation,
                cell_index,
                after,
                result,
            });
        });
    }

    pub(super) fn on_rewind_files_restored(
        &mut self,
        tui: &mut Tui,
        generation: SessionGeneration,
        cell_index: usize,
        after: ForkAfter,
        result: Result<(), String>,
    ) {
        if self.chat_widget.session_generation() != generation {
            return;
        }
        match result {
            Ok(()) => self.fork_conversation_at(tui, cell_index, after),
            Err(error) => self.chat_widget.add_error_message(error),
        }
    }
}
