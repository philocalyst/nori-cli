//! Focus-backwards: walking back to an earlier prompt, and rewinding, editing or forking from it.
//!
//! One model for all of it. The focus sits on a prompt of the conversation: the stage draws it as
//! an amber card with everything after it dimmed (see `stage::focus`), `[` and `]` (or the arrow
//! keys) move it between prompts, and a key says what to do with the prompt it is on:
//!
//! | key | what |
//! |---|---|
//! | `Enter`, `e` | edit the prompt in place; `Enter` again sends it, restoring the files of the turns it rolls back |
//! | `r` | rewind to it: restore the files, fork before it, and put it in the composer |
//! | `R` | the same, keeping the files as they are |
//! | `f`, `Space` | fork a subagent from its response |
//! | `Esc` | leave |
//!
//! The same model walks the prompts of a child's journal, where the conversation drawn is the
//! child's and the only actions are the ones the journal has (rollback and fork).

use std::sync::Arc;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use nori_config::KeyAction;
use nori_config::KeyMode;
use nori_harness::UndoSnapshot;

use super::App;
use crate::app_event::AppEvent;
use crate::app_event::ForkAfter;
use crate::app_event::SessionGeneration;
use crate::bottom_pane::textarea::TextArea;
use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::history_cell::UserHistoryCell;
use crate::pager_overlay::MessageAction;
use crate::pager_overlay::MessageChoice;
use crate::stage::EditView;
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
    /// The prompt, while it is being edited in place.
    editor: Option<TextArea>,
    /// What the edit journal says restoring would undo, once it answered.
    snapshots: Option<Vec<UndoSnapshot>>,
    /// Opened with `[`: a `t` right after toggles the nearest tool card instead.
    opened_by_bracket: bool,
}

impl Focus {
    pub(crate) fn editing(&self) -> bool {
        self.editor.is_some()
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

    /// Focus the last prompt of the conversation shown. For a child, the journal's cells are
    /// given; the main conversation is the transcript.
    pub(super) fn begin_focus(
        &mut self,
        tui: &mut Tui,
        child_cells: Vec<Arc<dyn HistoryCell>>,
        opened_by_bracket: bool,
    ) {
        let target = self.session_view.focused();
        let cells: &[Arc<dyn HistoryCell>] = if target == Target::Main {
            &self.transcript_cells
        } else {
            &child_cells
        };
        let Some(selected) = cells.iter().rposition(is_prompt) else {
            self.chat_widget
                .add_info_message("Nothing has been sent yet to rewind to.".to_owned(), None);
            return;
        };
        self.focus = Some(Focus {
            target,
            child_cells,
            selected,
            editor: None,
            snapshots: None,
            opened_by_bracket,
        });
        if target == Target::Main
            && let Some(handle) = self.chat_widget.harness_handle()
        {
            let tx = self.app_event_tx.clone();
            let generation = self.chat_widget.session_generation();
            tokio::spawn(async move {
                if let Ok(snapshots) = handle.undo_snapshots().await {
                    tx.send(AppEvent::FocusSnapshotsLoaded {
                        generation,
                        snapshots,
                    });
                }
            });
        }
        self.sync_focus_badge();
        tui.frame_requester().schedule_frame();
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
        self.sync_focus_badge();
        tui.frame_requester().schedule_frame();
    }

    /// Tell the status line which prompt is focused, if any.
    fn sync_focus_badge(&mut self) {
        let badge = self
            .focus
            .as_ref()
            .map(|focus| turn_position(self.focus_cells(), focus.selected));
        self.chat_widget.set_rewind_badge(badge);
    }

    /// The focused prompt as the stage draws it, for the main conversation.
    pub(crate) fn focus_view(&self) -> Option<FocusView> {
        let focus = self
            .focus
            .as_ref()
            .filter(|focus| focus.target == Target::Main)?;
        Some(self.focus_view_of(focus))
    }

    fn focus_view_of(&self, focus: &Focus) -> FocusView {
        let cells = self.focus_cells();
        FocusView {
            selected: focus.selected,
            turn: turn_position(cells, focus.selected),
            edit: focus.editor.as_ref().map(|editor| EditView {
                text: editor.text().to_owned(),
                cursor: editor.cursor(),
            }),
            undo_turns: focus.snapshots.as_ref().map(|snapshots| {
                crate::chatwidget::rewind::turns_to_undo(
                    snapshots,
                    &self.prompts_from(focus.selected),
                )
            }),
            has_downstream: focus.selected + 1 < cells.len(),
        }
    }

    /// A child's journal as drawn while one of its prompts is focused.
    pub(crate) fn focus_child_rows(&self, width: u16) -> Option<crate::stage::FocusedColumn> {
        let focus = self
            .focus
            .as_ref()
            .filter(|focus| focus.target != Target::Main)?;
        Some(crate::stage::render_focused(
            &self.focus_view_of(focus),
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

    /// Keep the focus honest: its prompt must still be one.
    pub(super) fn validate_focus(&mut self, tui: &mut Tui) {
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
        let editing = self.focus.as_ref().is_some_and(Focus::editing);
        if editing {
            self.handle_focus_edit_key(tui, key);
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
            KeyAction::FocusEdit => self.start_focus_edit(tui),
            KeyAction::FocusRewind => self.rewind_from_focus(tui, true),
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
                self.sync_focus_badge();
                tui.frame_requester().schedule_frame();
            }
            // Past the newest prompt is the composer again.
            None if !older => self.close_focus(tui),
            None => {}
        }
    }

    fn start_focus_edit(&mut self, tui: &mut Tui) {
        let Some(focus) = self.focus.as_ref() else {
            return;
        };
        if focus.target != Target::Main {
            self.rewind_from_focus(tui, true);
            return;
        }
        let text = self
            .prompts_from(focus.selected)
            .into_iter()
            .next()
            .unwrap_or_default();
        let mut editor = TextArea::new();
        editor.set_hotkey_config(self.hotkey_config.clone());
        editor.set_text(&text);
        editor.set_cursor(text.len());
        if let Some(focus) = self.focus.as_mut() {
            focus.editor = Some(editor);
        }
        tui.frame_requester().schedule_frame();
    }

    fn handle_focus_edit_key(&mut self, tui: &mut Tui, key: KeyEvent) {
        let Some(editor) = self.focus.as_mut().and_then(|focus| focus.editor.as_mut()) else {
            return;
        };
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                if let Some(focus) = self.focus.as_mut() {
                    focus.editor = None;
                }
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                let text = editor.text().trim().to_owned();
                if text.is_empty() {
                    if let Some(focus) = self.focus.as_mut() {
                        focus.editor = None;
                    }
                } else {
                    self.dispatch_rewind(tui, true, ForkAfter::Submit(text));
                }
            }
            _ => editor.input(key),
        }
        tui.frame_requester().schedule_frame();
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
        if self.chat_widget.is_task_running() {
            self.chat_widget.add_info_message(
                "A turn is running: stop it with Esc before rewinding.".to_owned(),
                None,
            );
            return;
        }
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
