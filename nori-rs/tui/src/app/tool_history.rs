//! Updates tool calls in their original chronological history slot.

use std::sync::Arc;

use crate::client_tool_cell::ClientToolCell;
use crate::history_cell::HistoryCell;
use crate::presentation::ToolSnapshot;

pub(super) fn update(cells: &mut [Arc<dyn HistoryCell>], snapshot: ToolSnapshot) -> Option<usize> {
    let (index, cell) = cells
        .iter_mut()
        .enumerate()
        .rev()
        .find_map(|(index, cell)| {
            let tool = cell.as_any().downcast_ref::<ClientToolCell>()?;
            (tool.call_id() == snapshot.call_id
                || tool.exploring_call_ids().contains(&snapshot.call_id))
            .then_some((index, cell))
        })?;
    let mut tool = cell.as_any().downcast_ref::<ClientToolCell>()?.clone();
    if tool.call_id() == snapshot.call_id {
        tool.apply_snapshot(snapshot);
    } else {
        tool.merge_exploring(snapshot);
    }
    *cell = Arc::new(tool);
    Some(index)
}

pub(super) fn fail_active(cells: &mut [Arc<dyn HistoryCell>], call_id: &str) -> Option<usize> {
    let (index, cell) = cells
        .iter_mut()
        .enumerate()
        .rev()
        .find_map(|(index, cell)| {
            let tool = cell.as_any().downcast_ref::<ClientToolCell>()?;
            (tool.call_id() == call_id && tool.is_active()).then_some((index, cell))
        })?;
    let mut tool = cell.as_any().downcast_ref::<ClientToolCell>()?.clone();
    tool.mark_failed();
    *cell = Arc::new(tool);
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::Artifact;
    use crate::presentation::Invocation;
    use crate::presentation::TextFormat;
    use crate::presentation::ToolKind;
    use crate::presentation::ToolPhase;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn interleaved_completion_replaces_original_cell_and_keeps_evidence() {
        let mut snapshot = ToolSnapshot {
            call_id: "call".to_owned(),
            title: "Executing command".to_owned(),
            kind: ToolKind::Execute,
            phase: ToolPhase::Pending,
            locations: Vec::new(),
            invocation: Some(Invocation::Command {
                command: "test".to_owned(),
            }),
            artifacts: Vec::new(),
            raw_input: None,
            raw_output: None,
            owner_request_id: None,
        };
        let tool = ClientToolCell::new(snapshot.clone(), PathBuf::from("/"), false);
        let mut cells: Vec<Arc<dyn HistoryCell>> = vec![
            Arc::new(tool),
            Arc::new(crate::history_cell::UserHistoryCell {
                message: "interleaved".to_owned(),
            }),
        ];
        snapshot.phase = ToolPhase::Completed;
        snapshot.raw_output = Some(serde_json::json!({"exit_code": 0}));
        snapshot.artifacts.push(Artifact::Text {
            text: "Command completed".to_owned(),
            format: TextFormat::Plain,
        });
        assert_eq!(update(&mut cells, snapshot), Some(0));
        assert_eq!(cells.len(), 2);
        let updated = cells[0]
            .as_any()
            .downcast_ref::<ClientToolCell>()
            .expect("tool");
        let compact = updated
            .compact_lines(80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        let expanded = updated
            .expanded_lines(80)
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            compact.contains("$ test") && compact.contains("exit 0"),
            "{compact}"
        );
        assert!(!compact.contains("Running"), "{compact}");
        assert!(expanded.contains("Command completed"), "{expanded}");
        assert!(expanded.contains('0'), "{expanded}");
        insta::assert_snapshot!(
            "completed_execute_in_history",
            format!("{compact}\n\n{expanded}")
        );
    }
}
