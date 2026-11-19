//! File edits: patch application events and the diffs they show.

use itertools::Itertools;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use crate::thread::client::SessionClient;
use crate::thread::prompt::PromptState;
use codex_protocol::protocol::FileChange;
use codex_protocol::protocol::PatchApplyBeginEvent;
use codex_protocol::protocol::PatchApplyEndEvent;
use codex_protocol::protocol::PatchApplyStatus;
use codex_protocol::protocol::PatchApplyUpdatedEvent;
use nori_acp_kit::schema::Content;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::Diff;
use nori_acp_kit::schema::TextContent;
use nori_acp_kit::schema::ToolCall;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolCallLocation;
use nori_acp_kit::schema::ToolCallStatus;
use nori_acp_kit::schema::ToolCallUpdate;
use nori_acp_kit::schema::ToolCallUpdateFields;
use nori_acp_kit::schema::ToolKind;

impl PromptState {
    pub(super) fn start_patch_apply(&self, client: &SessionClient, event: PatchApplyBeginEvent) {
        let raw_input = serde_json::json!(&event);
        let PatchApplyBeginEvent {
            call_id,
            auto_approved: _,
            changes,
            turn_id: _,
        } = event;

        let (title, locations, content) = extract_tool_call_content_from_changes(changes);

        client.send_tool_call(
            ToolCall::new(call_id, title)
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .locations(locations)
                .content(content.collect())
                .raw_input(raw_input),
        );
    }

    pub(super) fn update_patch_apply(&self, client: &SessionClient, event: PatchApplyUpdatedEvent) {
        let raw_input = serde_json::json!(&event);
        let PatchApplyUpdatedEvent { call_id, changes } = event;

        if changes.is_empty() {
            return;
        }

        let (title, locations, content) = extract_tool_call_content_from_changes(changes);

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .title(title)
                .locations(locations)
                .content(content.collect::<Vec<_>>())
                .raw_input(raw_input),
        ));
    }

    pub(super) fn end_patch_apply(&self, client: &SessionClient, event: PatchApplyEndEvent) {
        let raw_output = serde_json::json!(&event);
        let PatchApplyEndEvent {
            call_id,
            stdout: _,
            stderr: _,
            success,
            changes,
            turn_id: _,
            status,
        } = event;

        // A move's whole-file diffs need the old path, which is gone once the patch has landed;
        // the content reported before it landed stands.
        let has_move = changes.values().any(|change| {
            matches!(
                change,
                FileChange::Update {
                    move_path: Some(_),
                    ..
                }
            )
        });
        let (title, locations, content) = if !changes.is_empty() {
            let (title, locations, content) = extract_tool_call_content_from_changes(changes);
            (
                Some(title),
                Some(locations),
                (!has_move).then(|| content.collect()),
            )
        } else {
            (None, None, None)
        };

        let status = match status {
            PatchApplyStatus::Completed => ToolCallStatus::Completed,
            _ if success => ToolCallStatus::Completed,
            PatchApplyStatus::Failed | PatchApplyStatus::Declined => ToolCallStatus::Failed,
        };

        client.send_tool_call_update(ToolCallUpdate::new(
            call_id,
            ToolCallUpdateFields::new()
                .status(status)
                .raw_output(raw_output)
                .title(title)
                .locations(locations)
                .content(content),
        ));
    }
}

pub(super) fn extract_tool_call_content_from_changes(
    changes: HashMap<PathBuf, FileChange>,
) -> (
    String,
    Vec<ToolCallLocation>,
    impl Iterator<Item = ToolCallContent>,
) {
    let changes = changes.into_iter().collect_vec();
    let title = if changes.is_empty() {
        "Edit".to_string()
    } else {
        format!(
            "Edit {}",
            changes
                .iter()
                .map(|(path, change)| tool_call_location_for_change(path, change)
                    .display()
                    .to_string())
                .join(", ")
        )
    };
    let locations = changes
        .iter()
        .map(|(path, change)| ToolCallLocation::new(tool_call_location_for_change(path, change)))
        .collect_vec();
    let content = changes
        .into_iter()
        .flat_map(|(path, change)| extract_tool_call_content_from_change(path, change));

    (title, locations, content)
}

pub(super) fn tool_call_location_for_change(path: &Path, change: &FileChange) -> PathBuf {
    match change {
        FileChange::Update {
            move_path: Some(move_path),
            ..
        } => move_path.clone(),
        _ => path.to_path_buf(),
    }
}

pub(super) fn extract_tool_call_content_from_change(
    path: PathBuf,
    change: FileChange,
) -> Vec<ToolCallContent> {
    match change {
        FileChange::Add { content } => vec![ToolCallContent::Diff(Diff::new(path, content))],
        FileChange::Delete { content } => {
            vec![ToolCallContent::Diff(
                Diff::new(path, String::new()).old_text(content),
            )]
        }
        FileChange::Update {
            unified_diff,
            move_path: Some(destination),
        } => match moved_file_diffs(&path, &destination, &unified_diff) {
            Some(diffs) => diffs,
            None => extract_tool_call_content_from_unified_diff(destination, unified_diff),
        },
        FileChange::Update {
            unified_diff,
            move_path: None,
        } => extract_tool_call_content_from_unified_diff(path, unified_diff),
    }
}

/// A patch that moves a file, as the two whole-file edits it is: the old path emptied (its old
/// text recorded) and the destination created with the patched text. Fragments at the
/// destination alone could not be undone. Needs the file at its old path, so it exists only
/// before the patch lands; `None` afterwards or when the patch does not apply cleanly.
fn moved_file_diffs(
    path: &Path,
    destination: &Path,
    unified_diff: &str,
) -> Option<Vec<ToolCallContent>> {
    let old_text = std::fs::read_to_string(path).ok()?;
    // A pure rename has no hunks.
    let new_text = if unified_diff.trim().is_empty() {
        old_text.clone()
    } else {
        diffy::apply(&old_text, &diffy::Patch::from_str(unified_diff).ok()?).ok()?
    };
    Some(vec![
        ToolCallContent::Diff(Diff::new(path.to_path_buf(), String::new()).old_text(old_text)),
        ToolCallContent::Diff(Diff::new(destination.to_path_buf(), new_text)),
    ])
}

pub(super) fn extract_tool_call_content_from_unified_diff(
    path: PathBuf,
    unified_diff: String,
) -> Vec<ToolCallContent> {
    let Ok(patch) = diffy::Patch::from_str(&unified_diff) else {
        return vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(unified_diff),
        )))];
    };

    let diffs = patch
        .hunks()
        .iter()
        .map(|hunk| {
            let mut old_text = String::new();
            let mut new_text = String::new();

            for line in hunk.lines() {
                match line {
                    diffy::Line::Context(text) => {
                        old_text.push_str(text);
                        new_text.push_str(text);
                    }
                    diffy::Line::Delete(text) => old_text.push_str(text),
                    diffy::Line::Insert(text) => new_text.push_str(text),
                }
            }

            ToolCallContent::Diff(Diff::new(path.clone(), new_text).old_text(old_text))
        })
        .collect_vec();

    if diffs.is_empty() {
        vec![ToolCallContent::Content(Content::new(ContentBlock::Text(
            TextContent::new(unified_diff),
        )))]
    } else {
        diffs
    }
}
