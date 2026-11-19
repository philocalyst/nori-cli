//! Creating, editing, deleting and moving files: `● Edited src/lib.rs  +14 −4` over the diff.
//!
//! Collapsed, the diff is capped at a handful of rows and says how many it left out; expanded,
//! it is whole. Wide terminals get two panes, narrow ones a unified diff.

use ratatui::text::Line;
use ratatui::text::Span;

use super::ClientToolCell;
use crate::client_event_format::format_edit_tool_header;
use crate::client_event_format::relativize_paths_in_text;
use crate::diff_render::render_changes;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Disclosure;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Progress;
use crate::history_cell::chrome::Target;
use crate::history_cell::chrome::truncate_tail;
use crate::presentation::ToolKind;
use crate::presentation::ToolPhase;

/// Rows of diff a collapsed edit shows.
const COLLAPSED_ROWS: usize = 14;
/// Rows of diff an expanded edit shows at most.
const EXPANDED_ROWS: usize = 600;

pub(super) fn render(
    cell: &ClientToolCell,
    chrome: &Chrome,
    width: usize,
    disclosure: Disclosure,
) -> Vec<Line<'static>> {
    let snapshot = &cell.snapshot;
    let failed = snapshot.phase == ToolPhase::Failed;
    let active = cell.is_active();
    let is_move = snapshot.kind == ToolKind::Move;

    let cap = match disclosure {
        Disclosure::Collapsed => COLLAPSED_ROWS,
        Disclosure::Expanded => EXPANDED_ROWS,
    };
    // The one diff of the changes serves the header's counts and the body under it.
    let mut body = None;
    let waiting = cell.progress() == Progress::Waiting;
    let (verb, target, chip) = if cell.edit_changes.is_empty() {
        // No diff to read the change from: the call's own words.
        let header = relativize_paths_in_text(&format_edit_tool_header(snapshot), &cell.cwd);
        let (verb, rest) = match header.split_once(" failed: ") {
            Some((verb, rest)) => (format!("{verb} failed"), rest),
            None => {
                let (verb, rest) = header.split_once(' ').unwrap_or((header.as_str(), ""));
                (verb.to_owned(), rest)
            }
        };
        (verb, Target::Path(rest.to_owned()), Vec::new())
    } else {
        let (stats, drawn) = render_changes(&cell.edit_changes, &cell.cwd, width, Some(cap));
        body = Some(drawn);
        let verb = match (failed, active, is_move) {
            (false, true, _) if waiting => match (is_move, stats.verb) {
                (true, _) => "Move",
                (false, "Added") => "Add",
                (false, "Deleted") => "Delete",
                (false, _) => "Edit",
            }
            .to_owned(),
            (true, _, _) => match stats.verb {
                "Added" => "Add failed",
                "Deleted" => "Delete failed",
                _ => "Edit failed",
            }
            .to_owned(),
            (false, true, true) => "Moving".to_owned(),
            (false, true, false) => match stats.verb {
                "Added" => "Adding",
                "Deleted" => "Deleting",
                _ => "Editing",
            }
            .to_owned(),
            (false, false, true) => "Moved".to_owned(),
            (false, false, false) => stats.verb.to_owned(),
        };
        let target = match stats.path {
            Some(path) => Target::Path(path),
            None => Target::Text(format!("{} files", stats.files)),
        };
        (verb, target, chrome.line_counts(stats.added, stats.removed))
    };

    let mut lines = vec![chrome.header(
        Header {
            glyph: chrome.glyph(cell.progress(), cell.start_time, cell.animations_enabled),
            verb,
            target,
            chip,
            meta: if waiting {
                vec![Span::styled("awaiting approval", chrome.amber())]
            } else {
                Vec::new()
            },
        },
        width,
    )];

    if let Some(body) = body {
        lines.extend(body.lines);
        if body.hidden > 0 {
            lines.push(chrome.more_row(body.hidden, "lines"));
        }
    }
    // The header already says it failed; a row is only for a reason the call gave.
    if failed && let Some(reason) = super::error_text(snapshot) {
        let first = reason.lines().next().unwrap_or_default().trim();
        lines.push(Line::from(vec![
            chrome.body_prefix(),
            Span::styled(
                truncate_tail(first, width.saturating_sub(chrome.body_indent())),
                chrome.coral(),
            ),
        ]));
    }
    lines
}
