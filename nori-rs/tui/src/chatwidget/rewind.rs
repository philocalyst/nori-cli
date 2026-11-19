//! Rewinding to a prompt: the file edits of its turn and of every turn after it are restored from
//! the edit journal, then the conversation forks before the prompt with it in the composer.

use nori_harness::UndoSnapshot;

use super::ChatWidget;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::text_formatting::truncate_text;

/// Turns the harness opens without a prompt of the user's (`session_runtime_driver.rs`), which
/// belong with the prompt before them.
const UNPROMPTED_TURNS: [&str; 2] = ["goal continuation", "delegated task results"];

/// A rewind waiting for the edit journal.
pub(crate) struct Rewind {
    /// The transcript cell of the prompt rewound to.
    pub(crate) cell_index: usize,
    pub(crate) prompt: String,
    /// The prompts from that one to the newest, oldest first.
    pub(crate) prompts: Vec<String>,
}

impl ChatWidget {
    pub(crate) fn is_task_running(&self) -> bool {
        self.bottom_pane.is_task_running()
    }

    /// Ask the edit journal what the turns since `rewind.prompt` changed; the answer opens the
    /// confirmation.
    pub(crate) fn begin_rewind(&mut self, rewind: Rewind) {
        self.pending_rewind = Some(rewind);
        self.submit_harness_action(crate::app_event::HarnessAction::LoadUndoSnapshots);
    }

    /// Offer the rewind the journal's `snapshots` were loaded for, if one is waiting. Returns
    /// whether one was.
    pub(crate) fn offer_rewind(&mut self, snapshots: &[UndoSnapshot]) -> bool {
        let Some(rewind) = self.pending_rewind.take() else {
            return false;
        };
        let undone = turns_to_undo(snapshots, &rewind.prompts);
        let edit = {
            let tx = self.app_event_tx.clone();
            let (cell_index, prefill) = (rewind.cell_index, rewind.prompt.clone());
            move || {
                tx.send(AppEvent::ForkToMessage {
                    cell_index,
                    prefill: prefill.clone(),
                })
            }
        };
        let turns: Vec<String> = snapshots[..undone]
            .iter()
            .rev()
            .map(|snapshot| {
                format!(
                    "{} ({})",
                    truncate_text(&snapshot.label, 32),
                    snapshot.short_id
                )
            })
            .collect();
        let mut items = Vec::new();
        if let (Some(last), Some(handle)) = (undone.checked_sub(1), self.harness_handle()) {
            let tx = self.app_event_tx.clone();
            let (cell_index, prefill) = (rewind.cell_index, rewind.prompt.clone());
            let index = snapshots[last].index;
            items.push(item(
                "Restore files and edit the prompt",
                Some(turns.join(", ")),
                Box::new(move |_| {
                    let (handle, tx, prefill) = (handle.clone(), tx.clone(), prefill.clone());
                    tokio::spawn(async move {
                        // The files are back before the conversation that changed them ends.
                        match handle.undo_to(index).await {
                            Ok(()) => tx.send(AppEvent::ForkToMessage {
                                cell_index,
                                prefill,
                            }),
                            Err(error) => tx.send(AppEvent::HarnessActionFailed(format!(
                                "Rewind could not restore the files: {error:#}"
                            ))),
                        }
                    });
                }),
            ));
        }
        items.push(item(
            "Edit the prompt, keep the files",
            None,
            Box::new(move |_| edit()),
        ));
        items.push(item("Cancel", None, Box::new(|_| {})));
        let subtitle = match undone {
            0 => "No file edits were recorded for it or the prompts after it.".to_owned(),
            1 => "Restoring undoes what 1 turn changed".to_owned(),
            turns => format!("Restoring undoes what {turns} turns changed"),
        };
        self.bottom_pane.show_selection_view(SelectionViewParams {
            title: Some(format!("Rewind to “{}”", truncate_text(&rewind.prompt, 48))),
            subtitle: Some(subtitle),
            footer_hint: Some(standard_popup_hint_line()),
            items,
            header: Box::new(()),
            ..Default::default()
        });
        self.request_redraw();
        true
    }
}

fn item(
    name: &str,
    description: Option<String>,
    action: crate::bottom_pane::SelectionAction,
) -> SelectionItem {
    SelectionItem {
        name: name.to_owned(),
        display_shortcut: None,
        description,
        selected_description: None,
        is_current: false,
        actions: vec![action],
        dismiss_on_select: true,
        search_value: None,
        is_header: false,
        menu_tone: nori_tui_components::MenuItemTone::Default,
    }
}

/// How many of `snapshots` (undoable turns, newest first) belong to the turns of `prompts` (the
/// prompt rewound to and every one after it, oldest first). Snapshots are matched to prompts by
/// label, newest first, so a prompt sent twice is matched in order; a turn without a prompt of
/// the user's belongs with the prompt before it, and is undone while a listed prompt older than
/// it may remain. The first snapshot that matches no remaining prompt is older than the prompt
/// rewound to, and so is everything after it.
pub(crate) fn turns_to_undo(snapshots: &[UndoSnapshot], prompts: &[String]) -> usize {
    let mut remaining = prompts.len();
    snapshots
        .iter()
        .take_while(|snapshot| {
            if UNPROMPTED_TURNS.contains(&snapshot.label.as_str()) {
                return remaining > 0;
            }
            match prompts[..remaining]
                .iter()
                .rposition(|prompt| *prompt == snapshot.label)
            {
                // A prompt opens one turn; only the prompts before it own older ones.
                Some(position) => {
                    remaining = position;
                    true
                }
                None => false,
            }
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn snapshots(labels: &[&str]) -> Vec<UndoSnapshot> {
        labels
            .iter()
            .enumerate()
            .map(|(index, label)| UndoSnapshot {
                index: index as i64,
                short_id: "1 file".to_owned(),
                label: (*label).to_owned(),
            })
            .collect()
    }

    fn prompts(texts: &[&str]) -> Vec<String> {
        texts.iter().map(|text| (*text).to_owned()).collect()
    }

    #[test]
    fn the_turns_from_the_prompt_on_are_undone_and_no_older_one() {
        // Newest first: "fix tests" and "add docs" came after "refactor"; "setup" before it.
        let journal = snapshots(&["fix tests", "add docs", "refactor", "setup"]);
        assert_eq!(
            turns_to_undo(&journal, &prompts(&["refactor", "add docs", "fix tests"])),
            3
        );
        assert_eq!(turns_to_undo(&journal, &prompts(&["fix tests"])), 1);
        // A prompt whose turn changed no file has no snapshot of its own.
        assert_eq!(
            turns_to_undo(&journal, &prompts(&["explain", "fix tests"])),
            1
        );
    }

    #[test]
    fn a_prompt_sent_twice_is_matched_in_order() {
        let journal = snapshots(&["continue", "other", "continue"]);
        // Rewinding to the second "continue" leaves the first one's edits alone.
        assert_eq!(turns_to_undo(&journal, &prompts(&["continue"])), 1);
        assert_eq!(
            turns_to_undo(&journal, &prompts(&["continue", "other", "continue"])),
            3
        );
    }

    #[test]
    fn turns_without_a_prompt_go_with_the_prompt_before_them() {
        // The results came after "fix"; the goal continuation after "old", before "fix".
        let journal = snapshots(&["delegated task results", "fix", "goal continuation", "old"]);
        assert_eq!(turns_to_undo(&journal, &prompts(&["fix"])), 2);
        assert_eq!(turns_to_undo(&journal, &prompts(&["old", "fix"])), 4);
        assert_eq!(turns_to_undo(&journal, &[]), 0);
    }
}
