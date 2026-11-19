//! Edits to the transcript's cells that are not appends.

use std::path::Path;
use std::sync::Arc;

use crate::history_cell::AgentMarkdownCell;
use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;

/// Replace the streamed pieces of the last answer with one cell made from its full `source`, so
/// that it wraps as a whole at any width. Returns the range of cells it replaced and the cell that
/// replaced them.
pub(crate) fn consolidate_agent_message_cells(
    transcript_cells: &mut Vec<Arc<dyn HistoryCell>>,
    source: String,
    cwd: &Path,
) -> Option<(std::ops::Range<usize>, Arc<dyn HistoryCell>)> {
    let end = transcript_cells.len();
    let mut start = end;
    while start > 0 {
        let cell = &transcript_cells[start - 1];
        if !cell.as_any().is::<AgentMessageCell>() {
            break;
        }
        start -= 1;
        if !cell.is_stream_continuation() {
            break;
        }
    }
    if start == end {
        return None;
    }

    let range = start..end;
    let replacement = Arc::new(AgentMarkdownCell::new(source, cwd)) as Arc<dyn HistoryCell>;
    transcript_cells.splice(range.clone(), std::iter::once(replacement.clone()));
    Some((range, replacement))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_cell::PlainHistoryCell;
    use pretty_assertions::assert_eq;
    use ratatui::text::Line;

    #[test]
    fn consolidation_replaces_the_streamed_pieces_and_keeps_their_neighbours() {
        let mut cells: Vec<Arc<dyn HistoryCell>> = vec![
            Arc::new(PlainHistoryCell::new(vec![Line::from("tool")])),
            Arc::new(AgentMessageCell::new(vec![Line::from("first")], true)),
            Arc::new(AgentMessageCell::new(vec![Line::from("second")], false)),
        ];

        let consolidation = consolidate_agent_message_cells(
            &mut cells,
            "first paragraph\n\nsecond paragraph".to_string(),
            Path::new("/tmp"),
        );

        assert_eq!(consolidation.map(|(range, _)| range), Some(1..3));
        assert_eq!(cells.len(), 2);
        assert!(cells[1].as_any().is::<AgentMarkdownCell>());
    }
}
