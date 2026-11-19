//! The file changes a tool call carries, as the diff renderer wants them: from its diff
//! artifacts when it has them, else from its invocation.

use std::collections::HashMap;
use std::path::PathBuf;

use nori_harness::patch::first_line_in_file;

use crate::presentation::ToolSnapshot;

pub(crate) fn diff_changes_from_artifacts(
    artifacts: &[crate::presentation::Artifact],
    cwd: &std::path::Path,
) -> std::collections::HashMap<std::path::PathBuf, crate::ui_types::FileChange> {
    let mut changes = std::collections::HashMap::new();
    for artifact in artifacts {
        if let crate::presentation::Artifact::Diff(change) = artifact {
            let file_change = match &change.old_text {
                None => crate::ui_types::FileChange::Add {
                    content: change.new_text.clone(),
                },
                Some(old_text) => crate::ui_types::FileChange::Update {
                    first_line: first_line_in_file(&change.path, cwd, old_text, &change.new_text),
                    old_text: old_text.clone(),
                    new_text: change.new_text.clone(),
                    move_path: None,
                },
            };
            changes.insert(change.path.clone(), file_change);
        }
    }
    changes
}

pub(crate) fn changes_from_invocation(
    invocation: &Option<crate::presentation::Invocation>,
    cwd: &std::path::Path,
) -> std::collections::HashMap<std::path::PathBuf, crate::ui_types::FileChange> {
    let mut changes = std::collections::HashMap::new();
    match invocation.as_ref() {
        Some(crate::presentation::Invocation::FileChanges { changes: fc }) => {
            for change in fc {
                let file_change = match &change.old_text {
                    None => crate::ui_types::FileChange::Add {
                        content: change.new_text.clone(),
                    },
                    Some(old_text) => crate::ui_types::FileChange::Update {
                        first_line: first_line_in_file(
                            &change.path,
                            cwd,
                            old_text,
                            &change.new_text,
                        ),
                        old_text: old_text.clone(),
                        new_text: change.new_text.clone(),
                        move_path: None,
                    },
                };
                changes.insert(change.path.clone(), file_change);
            }
        }
        Some(crate::presentation::Invocation::FileOperations { operations }) => {
            for op in operations {
                let (path, file_change) = match op {
                    crate::presentation::FileOperation::Create { path, new_text } => (
                        path.clone(),
                        crate::ui_types::FileChange::Add {
                            content: new_text.clone(),
                        },
                    ),
                    crate::presentation::FileOperation::Update {
                        path,
                        old_text,
                        new_text,
                    } => (
                        path.clone(),
                        crate::ui_types::FileChange::Update {
                            first_line: first_line_in_file(path, cwd, old_text, new_text),
                            old_text: old_text.clone(),
                            new_text: new_text.clone(),
                            move_path: None,
                        },
                    ),
                    crate::presentation::FileOperation::Delete { path, old_text } => (
                        path.clone(),
                        crate::ui_types::FileChange::Delete {
                            content: old_text.clone().unwrap_or_default(),
                        },
                    ),
                    crate::presentation::FileOperation::Move {
                        from_path,
                        to_path,
                        old_text,
                        new_text,
                    } => {
                        let old = old_text.clone().unwrap_or_default();
                        let new = new_text.clone().unwrap_or_else(|| old.clone());
                        (
                            from_path.clone(),
                            crate::ui_types::FileChange::Update {
                                first_line: first_line_in_file(from_path, cwd, &old, &new),
                                old_text: old,
                                new_text: new,
                                move_path: Some(to_path.clone()),
                            },
                        )
                    }
                };
                changes.insert(path, file_change);
            }
        }
        _ => {}
    }
    changes
}

pub(super) fn changes_from_snapshot(
    snapshot: &ToolSnapshot,
    cwd: &std::path::Path,
) -> HashMap<PathBuf, crate::ui_types::FileChange> {
    let diff_changes = diff_changes_from_artifacts(&snapshot.artifacts, cwd);
    if diff_changes.is_empty() {
        changes_from_invocation(&snapshot.invocation, cwd)
    } else {
        diff_changes
    }
}
