//! Reads, searches and listings: the calls that look at the code rather than change it.
//!
//! One of them is one line (`● Read src/app.rs:120–180`). Several in a row are one card: a
//! header that counts them (`● Explored · 5 files · 2 searches`) over a bento grid with a tile
//! each, instead of a list the eye has to walk down.

use std::path::Path;
use std::path::PathBuf;

use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

use super::ClientToolCell;
use crate::history_cell::bento::Tile;
use crate::history_cell::bento::grid;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Disclosure;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Progress;
use crate::history_cell::chrome::Target;
use crate::history_cell::chrome::format_duration;
use crate::presentation::Artifact;
use crate::presentation::Invocation;
use crate::presentation::ToolKind;
use crate::presentation::ToolPhase;
use crate::presentation::ToolSnapshot;
use crate::render::highlight::highlight_code_to_lines;
use crate::render::highlight::highlight_code_to_lines_for_path;

/// Rows of tiles a collapsed card shows.
const COLLAPSED_ROWS: usize = 3;

/// The lines a read covers, as the call asked for them.
pub(super) struct ReadSpan {
    pub(super) start: usize,
    pub(super) end: Option<usize>,
}

impl ReadSpan {
    /// `10–40`, or `10` for one line.
    pub(super) fn label(&self) -> String {
        match self.end {
            Some(end) if end > self.start => format!("{}–{end}", self.start),
            _ => self.start.to_string(),
        }
    }
}

/// Which lines of a file a read call covered, from its input or, failing that, its location.
pub(super) fn read_span(snapshot: &ToolSnapshot) -> Option<ReadSpan> {
    let input = |key: &str| {
        snapshot
            .raw_input
            .as_ref()
            .and_then(|input| input.get(key))
            .and_then(Value::as_u64)
            .map(|value| usize::try_from(value).unwrap_or(usize::MAX))
    };
    let start = input("offset")
        .or_else(|| input("start_line"))
        .or_else(|| input("line"))
        .or_else(|| {
            snapshot
                .locations
                .first()
                .and_then(|location| location.line)
                .map(|line| usize::try_from(line).unwrap_or(usize::MAX))
        })?
        .max(1);
    let end = input("end_line")
        .or_else(|| input("limit").map(|limit| start.saturating_add(limit.saturating_sub(1))));
    Some(ReadSpan { start, end })
}

/// A path as the transcript says it: relative to the working directory, `~/` under home,
/// otherwise as it is.
fn short_path(path: &Path, cwd: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(cwd) {
        return relative.display().to_string();
    }
    match crate::exec_command::relativize_to_home(path) {
        Some(relative) => format!("~/{}", relative.display()),
        None => path.display().to_string(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Read,
    Search,
    List,
    Other,
}

/// Rows of what a call returned that an expanded tile shows.
const EXCERPT_ROWS: usize = 3;
/// Tiles an expanded card shows before it counts the rest.
const EXPANDED_ROWS: usize = 6;

/// One call, reduced to what a tile or a one-line header says.
struct Entry {
    kind: EntryKind,
    head: String,
    path_like: bool,
    tail: String,
    progress: Progress,
    /// The query of a search.
    query: Option<String>,
    /// Where a read's excerpt starts, and the file it is from (which says how to colour it).
    first_line: usize,
    file: Option<PathBuf>,
    /// The first rows of what it returned, and how many rows it returned in all.
    excerpt: Vec<String>,
    returned: usize,
}

fn entry_of(snapshot: &ToolSnapshot, cwd: &Path, with_excerpt: bool) -> Entry {
    let progress = progress_of(&snapshot.phase);
    let mut query = None;
    let mut file = None;
    let (kind, head, path_like, tail) = match (&snapshot.kind, &snapshot.invocation) {
        (ToolKind::Read, Some(Invocation::Read { path })) => {
            file = Some(path.clone());
            (
                EntryKind::Read,
                short_path(path, cwd),
                true,
                read_span(snapshot).map_or_else(String::new, |span| format!(":{}", span.label())),
            )
        }
        (_, Some(Invocation::Search { query: text, path })) => {
            query.clone_from(text);
            (
                EntryKind::Search,
                text.clone()
                    .map_or_else(|| "search".to_owned(), |text| format!("“{text}”")),
                false,
                path.as_ref()
                    .map_or_else(String::new, |path| format!(" in {}", short_path(path, cwd))),
            )
        }
        (_, Some(Invocation::ListFiles { path })) => (
            EntryKind::List,
            path.as_ref()
                .map_or_else(|| ".".to_owned(), |path| short_path(path, cwd)),
            true,
            String::new(),
        ),
        (ToolKind::Read, _) => (
            EntryKind::Read,
            crate::client_event_format::relativize_paths_in_text(&snapshot.title, cwd),
            false,
            String::new(),
        ),
        (kind, _) => (
            if *kind == ToolKind::Search {
                EntryKind::Search
            } else {
                EntryKind::Other
            },
            crate::client_event_format::relativize_paths_in_text(&snapshot.title, cwd),
            false,
            String::new(),
        ),
    };
    let returned_text = output_text(snapshot).unwrap_or_default();
    let rows: Vec<&str> = returned_text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    // The rows that are the result: a search's lines with its query in them (all of them when
    // none has it, as for a listing). The count in the header and the rows under it both come
    // from these; a tool that reports a different total says so.
    let listed: Vec<&str> = match (kind, &query) {
        (EntryKind::Search, Some(query)) => {
            let needle = query.to_lowercase();
            let hits: Vec<&str> = rows
                .iter()
                .copied()
                .filter(|line| line.to_lowercase().contains(&needle))
                .collect();
            if hits.is_empty() { rows.clone() } else { hits }
        }
        _ => rows.clone(),
    };
    let returned = reported_total(snapshot).unwrap_or(listed.len());
    let excerpt = if with_excerpt {
        listed
            .iter()
            .take(EXCERPT_ROWS)
            .map(|line| (*line).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    Entry {
        kind,
        head,
        path_like,
        tail,
        progress,
        query,
        first_line: read_span(snapshot).map_or(1, |span| span.start),
        file,
        excerpt,
        returned,
    }
}

/// The number of results a tool says it found, when it says (`total`, `count`, `numMatches`).
fn reported_total(snapshot: &ToolSnapshot) -> Option<usize> {
    let object = match snapshot.raw_output.as_ref()? {
        Value::Object(object) => object.clone(),
        Value::String(text) => match serde_json::from_str::<Value>(text).ok()? {
            Value::Object(object) => object,
            _ => return None,
        },
        _ => return None,
    };
    ["total", "count", "numMatches", "num_matches", "matchCount"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_u64))
        .and_then(|total| usize::try_from(total).ok())
}

/// The text a call returned: its output, or failing that its text artifact.
fn output_text(snapshot: &ToolSnapshot) -> Option<String> {
    fn of_value(value: &Value) -> Option<String> {
        match value {
            Value::String(text) => match serde_json::from_str::<Value>(text) {
                Ok(parsed @ Value::Object(_)) => of_value(&parsed),
                Ok(_) | Err(_) => Some(text.clone()),
            },
            Value::Object(object) => ["content", "text", "output", "stdout", "result"]
                .iter()
                .find_map(|key| object.get(*key).and_then(Value::as_str).map(str::to_owned))
                .or_else(|| {
                    ["lines", "matches"]
                        .iter()
                        .find_map(|key| object.get(*key).and_then(Value::as_array))
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .collect::<Vec<_>>()
                                .join("\n")
                        })
                }),
            Value::Array(items) => Some(
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Value::Null | Value::Bool(_) | Value::Number(_) => None,
        }
    }
    snapshot
        .raw_output
        .as_ref()
        .and_then(of_value)
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            snapshot
                .artifacts
                .iter()
                .find_map(|artifact| match artifact {
                    Artifact::Text { text, .. } => Some(text.clone()),
                    Artifact::Diff(_) => None,
                })
        })
}

/// A tile's excerpt as spans: a read's lines numbered and coloured as the file's language, a
/// search's lines with the hit stood out.
fn excerpt_rows(chrome: &Chrome, entry: &Entry) -> Vec<Vec<Span<'static>>> {
    match entry.kind {
        EntryKind::Read => {
            let joined = entry.excerpt.join("\n");
            let coloured = match &entry.file {
                Some(file) => highlight_code_to_lines_for_path(&joined, file),
                None => highlight_code_to_lines(&joined, "text"),
            };
            entry
                .excerpt
                .iter()
                .enumerate()
                .map(|(index, plain)| {
                    let mut spans = vec![Span::styled(
                        format!("{:>3} ", entry.first_line + index),
                        chrome.faint(),
                    )];
                    match coloured.get(index) {
                        Some(line) if !line.spans.is_empty() => {
                            spans.extend(line.spans.iter().cloned());
                        }
                        Some(_) | None => spans.push(Span::raw(plain.clone())),
                    }
                    spans
                })
                .collect()
        }
        EntryKind::Search => entry
            .excerpt
            .iter()
            .map(|line| highlight_hits(chrome, line.trim(), entry.query.as_deref()))
            .collect(),
        EntryKind::List | EntryKind::Other => entry
            .excerpt
            .iter()
            .map(|line| vec![Span::styled(line.trim().to_owned(), chrome.muted())])
            .collect(),
    }
}

/// `line` with each occurrence of `query` (any case) stood out.
fn highlight_hits(chrome: &Chrome, line: &str, query: Option<&str>) -> Vec<Span<'static>> {
    let plain = |text: &str| Span::styled(text.to_owned(), chrome.secondary());
    let Some(query) = query.filter(|query| !query.is_empty()) else {
        return vec![plain(line)];
    };
    let (lower, needle) = (line.to_lowercase(), query.to_lowercase());
    // Lowercasing can change byte lengths; positions are only trusted when it did not.
    if lower.len() != line.len() {
        return vec![plain(line)];
    }
    let hit = chrome.theme.diff_add_word.add_modifier(Modifier::BOLD);
    let mut spans = Vec::new();
    let mut at = 0;
    while let Some(found) = lower[at..].find(&needle) {
        let (start, end) = (at + found, at + found + needle.len());
        if !line.is_char_boundary(start) || !line.is_char_boundary(end) {
            break;
        }
        if start > at {
            spans.push(plain(&line[at..start]));
        }
        spans.push(Span::styled(line[start..end].to_owned(), hit));
        at = end;
    }
    if at < line.len() {
        spans.push(plain(&line[at..]));
    }
    spans
}

fn progress_of(phase: &ToolPhase) -> Progress {
    match phase {
        ToolPhase::Pending | ToolPhase::InProgress => Progress::Running,
        ToolPhase::PendingApproval => Progress::Waiting,
        ToolPhase::Completed => Progress::Done,
        ToolPhase::Failed => Progress::Failed,
    }
}

pub(super) fn render(
    cell: &ClientToolCell,
    chrome: &Chrome,
    width: usize,
    disclosure: Disclosure,
) -> Vec<Line<'static>> {
    let expanded = disclosure == Disclosure::Expanded;
    let snapshots: &[ToolSnapshot] = if cell.exploring_snapshots.is_empty() {
        std::slice::from_ref(&cell.snapshot)
    } else {
        &cell.exploring_snapshots
    };
    let entries: Vec<Entry> = snapshots
        .iter()
        .map(|snapshot| entry_of(snapshot, &cell.cwd, expanded))
        .collect();
    let progress = if entries
        .iter()
        .any(|entry| entry.progress == Progress::Running)
    {
        Progress::Running
    } else if entries
        .iter()
        .any(|entry| entry.progress == Progress::Failed)
    {
        Progress::Failed
    } else if entries
        .iter()
        .any(|entry| entry.progress == Progress::Waiting)
    {
        Progress::Waiting
    } else {
        Progress::Done
    };
    let running = progress == Progress::Running;

    let mut lines = Vec::new();
    if let [only] = &entries[..] {
        // One call is one header, led by what kind of look it was; its excerpt, when expanded,
        // is under it.
        let glyph = if running {
            chrome.glyph(Progress::Running, cell.start_time, cell.animations_enabled)
        } else {
            Span::styled(
                match only.kind {
                    EntryKind::Read => "◈",
                    EntryKind::Search => "⌕",
                    EntryKind::List => "▤",
                    EntryKind::Other => "●",
                },
                chrome.tone(progress),
            )
        };
        let (verb, target) = match only.kind {
            EntryKind::Read => ("", Target::Path(format!("{}{}", only.head, only.tail))),
            EntryKind::Search => ("", Target::Text(format!("{}{}", only.head, only.tail))),
            EntryKind::List => ("List", Target::Path(only.head.clone())),
            EntryKind::Other => ("", Target::Text(only.head.clone())),
        };
        let mut meta: Vec<Span<'static>> = Vec::new();
        if only.kind == EntryKind::Search && only.returned > 0 {
            // Expanded, the rows shown are counted against the total the tool found.
            let shown = only.excerpt.len();
            meta.push(chrome.fact(if expanded && shown < only.returned {
                format!("{shown} shown of {}", only.returned)
            } else {
                match only.returned {
                    1 => "1 match".to_owned(),
                    count => format!("{count} matches"),
                }
            }));
        }
        meta.extend(
            cell.elapsed
                .and_then(format_duration)
                .map(|took| chrome.fact(took)),
        );
        lines.push(chrome.header(
            Header {
                glyph,
                verb: verb.to_owned(),
                target,
                chip: Vec::new(),
                meta,
            },
            width,
        ));
        if expanded {
            let rows = excerpt_rows(chrome, only);
            lines.extend(chrome.well(rows, width));
        }
    } else {
        let glyph = chrome.glyph(progress, cell.start_time, cell.animations_enabled);
        let count = |kind: EntryKind| entries.iter().filter(|entry| entry.kind == kind).count();
        let mut meta = Vec::new();
        for (kind, one, many) in [
            (EntryKind::Read, "file", "files"),
            (EntryKind::Search, "search", "searches"),
            (EntryKind::List, "listing", "listings"),
            (EntryKind::Other, "call", "calls"),
        ] {
            match count(kind) {
                0 => {}
                1 => meta.push(chrome.fact(format!("1 {one}"))),
                many_count => meta.push(chrome.fact(format!("{many_count} {many}"))),
            }
        }
        lines.push(chrome.header(
            Header {
                glyph,
                verb: if running { "Exploring" } else { "Explored" }.to_owned(),
                target: Target::None,
                chip: Vec::new(),
                meta,
            },
            width,
        ));
        let tiles = entries
            .into_iter()
            .map(|entry| Tile {
                glyph: match entry.progress {
                    Progress::Running => {
                        chrome.glyph(Progress::Running, cell.start_time, cell.animations_enabled)
                    }
                    Progress::Failed => Span::styled("✗", chrome.coral()),
                    Progress::Waiting | Progress::Done | Progress::Cancelled | Progress::Idle => {
                        Span::styled(
                            match entry.kind {
                                EntryKind::Read => "◈",
                                EntryKind::Search => "⌕",
                                EntryKind::List => "▤",
                                EntryKind::Other => "·",
                            },
                            chrome.cyan(),
                        )
                    }
                },
                excerpt: excerpt_rows(chrome, &entry),
                head: entry.head,
                path_like: entry.path_like,
                tail: entry.tail,
            })
            .collect();
        lines.extend(grid(
            chrome,
            tiles,
            width,
            Some(if expanded {
                EXPANDED_ROWS
            } else {
                COLLAPSED_ROWS
            }),
        ));
    }
    lines
}
