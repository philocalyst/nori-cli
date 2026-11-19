//! The fork picker: branch at the current point, or rewind to an earlier message.

use crate::app_event::AppEvent;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;

/// Maximum characters to show in a message preview in the picker.
const MAX_PREVIEW_CHARS: usize = 80;

/// Truncate a message to a single-line preview suitable for the picker.
fn truncate_preview(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or("");
    if first_line.chars().count() > MAX_PREVIEW_CHARS {
        let truncated: String = first_line.chars().take(MAX_PREVIEW_CHARS).collect();
        format!("{truncated}…")
    } else if message.lines().count() > 1 {
        format!("{first_line}…")
    } else {
        first_line.to_string()
    }
}

/// Label for the branch-at-head entry, always the first item in the picker.
const BRANCH_FROM_CURRENT_LABEL: &str = "⎇ Branch from current point";

/// The fork choices. `messages` are `(cell_index, message_text)` oldest first; they are listed
/// newest first. When the agent advertises ACP `session/fork` the first choice branches at the
/// current head.
pub(crate) fn fork_choices(messages: Vec<(usize, String)>, supports_fork: bool) -> ChoiceSpec {
    let mut items: Vec<ChoiceItem> = Vec::new();
    if supports_fork {
        items.push(ChoiceItem::run(
            BRANCH_FROM_CURRENT_LABEL,
            Box::new(|tx| tx.send(AppEvent::BranchFromCurrent)),
        ));
    }
    items.extend(messages.into_iter().rev().map(|(cell_index, message)| {
        ChoiceItem::run(
            truncate_preview(&message),
            Box::new(move |tx| {
                tx.send(AppEvent::ForkToMessage {
                    cell_index,
                    prefill: message.clone(),
                })
            }),
        )
    }));
    let mut spec = ChoiceSpec::new("Fork conversation", items);
    spec.subtitle = Some(
        if supports_fork {
            "Branch at the current point, or select a message to rewind to"
        } else {
            "Select a message to rewind to"
        }
        .to_owned(),
    );
    spec
}
