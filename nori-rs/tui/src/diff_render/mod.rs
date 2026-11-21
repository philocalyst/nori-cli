//! File changes drawn as diffs, the way a document editor shows suggestions: one column of the
//! new text under a faint line-number gutter, in syntax colours, every edited line merged in place
//! (what it lost struck through, what it gained marked) and a bar in the margin of each line that
//! changed. Edits are diffed with disparate ([`update`]); [`line`] draws a line and [`style`]
//! takes every colour from the live theme.
//!
//! The edit cell draws the one-line header itself and asks this module for the body: its rows,
//! capped, and how many it left out.

mod line;
mod style;
mod update;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use self::line::DiffLineLayout;
use self::line::Mark;
use self::line::diff_line_rows;
use self::line::line_number_width;
use self::style::DiffRenderStyleContext;
use self::update::EditDiff;
use crate::exec_command::relativize_to_home;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Progress;
use crate::history_cell::chrome::Target;
use crate::render::highlight::highlight_code_to_lines_for_path;
use crate::render::line_utils::prefix_lines;
use crate::render::renderable::Renderable;
use crate::ui_types::FileChange;

/// What a set of changes comes to, for a header.
pub(crate) struct DiffStats {
    pub(crate) files: usize,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    /// For one file, what was done to it; for several, `Edited`.
    pub(crate) verb: &'static str,
    /// For one file, where it is (and where it went, if it moved).
    pub(crate) path: Option<String>,
}

/// The rows of a diff body and how many further rows were left out of it.
pub(crate) struct DiffBody {
    pub(crate) lines: Vec<RtLine<'static>>,
    pub(crate) hidden: usize,
}

/// The changes as a header's counts and a body, from one diff of each file. The body is within
/// `width`; several files each get a faint `± path +n −m` row first, and at most `cap` rows are
/// returned when it is set.
pub(crate) fn render_changes(
    changes: &HashMap<PathBuf, FileChange>,
    cwd: &Path,
    width: usize,
    cap: Option<usize>,
) -> (DiffStats, DiffBody) {
    let chrome = Chrome::live();
    let ctx = DiffRenderStyleContext::detected();
    let rows = collect_rows(changes);
    let (verb, path) = match &rows[..] {
        [row] => (
            match &row.body {
                Body::Added(_) => "Added",
                Body::Deleted(_) => "Deleted",
                Body::Edited { .. } => "Edited",
            },
            Some(row_path(row, cwd)),
        ),
        _ => ("Edited", None),
    };
    let stats = DiffStats {
        files: rows.len(),
        added: rows.iter().map(|row| row.added).sum(),
        removed: rows.iter().map(|row| row.removed).sum(),
        verb,
        path,
    };
    let prefix = chrome.body_prefix();
    let inner = width.saturating_sub(chrome.body_indent()).max(1);
    let several = rows.len() > 1;
    let mut lines: Vec<RtLine<'static>> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        if several {
            if index > 0 {
                lines.push(RtLine::default());
            }
            let mut spans = vec![prefix.clone(), RtSpan::styled("± ", chrome.faint())];
            spans.extend(crate::history_cell::chrome::path_spans(
                &row_path(row, cwd),
                &chrome,
            ));
            spans.push(RtSpan::raw(" "));
            spans.extend(chrome.line_counts(row.added, row.removed));
            lines.push(RtLine::from(spans));
        }
        let mut body = Vec::new();
        render_body(row, &mut body, inner, &ctx);
        lines.extend(prefix_lines(body, prefix.clone(), prefix.clone()));
        // Past the cap nothing more is needed; the rest of the files are not drawn.
        if cap.is_some_and(|cap| lines.len() > cap + 64) {
            break;
        }
    }
    let hidden = cap.map_or(0, |cap| lines.len().saturating_sub(cap));
    if hidden > 0 {
        lines.truncate(lines.len() - hidden);
    }
    (stats, DiffBody { lines, hidden })
}

/// What a set of changes shown on its own is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangesShown {
    /// What an edit would do, waiting for an approval.
    Proposal,
    /// What the working tree holds that its base does not (`/diff`).
    WorkingTree,
}

/// The header and the whole body of a set of changes, for places that show a diff on its own
/// (an approval, `/diff`, a transcript detail).
pub(crate) fn create_diff_summary(
    changes: &HashMap<PathBuf, FileChange>,
    cwd: &Path,
    wrap_cols: usize,
    shown: ChangesShown,
) -> Vec<RtLine<'static>> {
    let chrome = Chrome::live();
    let (stats, body) = render_changes(changes, cwd, wrap_cols, None);
    let target = match &stats.path {
        Some(path) => Target::Path(path.clone()),
        None => Target::Text(format!("{} files", stats.files)),
    };
    let (glyph, verb) = match shown {
        ChangesShown::Proposal => (
            chrome.glyph(Progress::Waiting, None, false),
            match stats.verb {
                "Added" => "Add",
                "Deleted" => "Delete",
                _ => "Edit",
            },
        ),
        ChangesShown::WorkingTree => (
            RtSpan::styled("±", chrome.faint()),
            match stats.verb {
                "Added" => "Added",
                "Deleted" => "Deleted",
                _ => "Changed",
            },
        ),
    };
    let mut lines = vec![chrome.header(
        Header {
            glyph,
            verb: verb.to_owned(),
            target,
            chip: chrome.line_counts(stats.added, stats.removed),
            meta: Vec::new(),
        },
        wrap_cols,
    )];
    lines.extend(body.lines);
    lines
}

/// A set of changes as a [`Renderable`] that lays itself out at the width it is given.
pub struct DiffSummary {
    changes: HashMap<PathBuf, FileChange>,
    cwd: PathBuf,
    shown: ChangesShown,
}

impl DiffSummary {
    pub(crate) fn new(
        changes: HashMap<PathBuf, FileChange>,
        cwd: PathBuf,
        shown: ChangesShown,
    ) -> Self {
        Self {
            changes,
            cwd,
            shown,
        }
    }
}

impl Renderable for DiffSummary {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = create_diff_summary(
            &self.changes,
            &self.cwd,
            usize::from(area.width),
            self.shown,
        );
        Paragraph::new(lines).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        let lines = create_diff_summary(&self.changes, &self.cwd, usize::from(width), self.shown);
        u16::try_from(lines.len()).unwrap_or(u16::MAX)
    }
}

fn row_path(row: &Row<'_>, cwd: &Path) -> String {
    let mut path = display_path_for(row.path, cwd);
    if let Some(move_path) = row.move_path {
        path.push_str(&format!(" → {}", display_path_for(move_path, cwd)));
    }
    path
}

// Shared row for per-file presentation
struct Row<'a> {
    path: &'a Path,
    move_path: Option<&'a Path>,
    /// The path whose language the file is read in: its new name, when it moved.
    syntax_path: &'a Path,
    added: usize,
    removed: usize,
    body: Body<'a>,
}

/// What a file's change shows: all of its lines, or an edit's hunks.
enum Body<'a> {
    Added(&'a str),
    Deleted(&'a str),
    /// An update, diffed once for both its counts and its lines.
    Edited {
        diff: EditDiff<'a>,
        first_line: usize,
    },
}

impl<'a> Row<'a> {
    fn new(path: &'a Path, change: &'a FileChange) -> Self {
        let move_path = match change {
            FileChange::Update { move_path, .. } => move_path.as_deref(),
            FileChange::Add { .. } | FileChange::Delete { .. } => None,
        };
        let syntax_path = move_path.unwrap_or(path);
        let (added, removed, body) = match change {
            FileChange::Add { content } => (content.lines().count(), 0, Body::Added(content)),
            FileChange::Delete { content } => (0, content.lines().count(), Body::Deleted(content)),
            FileChange::Update {
                old_text,
                new_text,
                first_line,
                ..
            } => {
                let diff = EditDiff::new(old_text, new_text, syntax_path);
                let changed = diff.changed();
                let first_line = *first_line;
                (
                    changed.after,
                    changed.before,
                    Body::Edited { diff, first_line },
                )
            }
        };
        Self {
            path,
            move_path,
            syntax_path,
            added,
            removed,
            body,
        }
    }
}

fn collect_rows(changes: &HashMap<PathBuf, FileChange>) -> Vec<Row<'_>> {
    let mut rows: Vec<Row<'_>> = changes
        .iter()
        .map(|(path, change)| Row::new(path, change))
        .collect();
    rows.sort_by_key(|r| r.path);
    rows
}

/// The lines of one file's change, within `width`. A file that is new or gone throughout is its
/// code with the bar beside it, not a block of colour.
fn render_body(
    row: &Row<'_>,
    out: &mut Vec<RtLine<'static>>,
    width: usize,
    ctx: &DiffRenderStyleContext,
) {
    let (mark, content) = match &row.body {
        Body::Added(content) => (Mark::Added, *content),
        Body::Deleted(content) => (Mark::Removed, *content),
        Body::Edited { diff, first_line } => {
            return diff.render(*first_line, row.syntax_path, width, ctx, out);
        }
    };
    // A new file's every line is a change, and highlighting a long one is the cost of showing
    // it: past this many rows the rest is counted, not drawn.
    const MAX_FILE_ROWS: usize = 400;
    let total = content.lines().count();
    let shown = total.min(MAX_FILE_ROWS);
    let content = if shown < total {
        let end = content
            .lines()
            .take(shown)
            .map(|line| line.len() + 1)
            .sum::<usize>();
        &content[..end.min(content.len())]
    } else {
        content
    };
    let syntax_lines = highlighted_diff_lines_for_path(content, row.syntax_path);
    let layout = DiffLineLayout {
        width,
        line_number_width: line_number_width(total),
    };
    for (i, raw) in content.lines().enumerate() {
        let spans = match syntax_lines.as_ref().and_then(|lines| lines.get(i)) {
            Some(spans) if mark == Mark::Removed => spans
                .iter()
                .map(|span| span.clone().patch_style(Modifier::DIM))
                .collect(),
            Some(spans) => spans.clone(),
            None if mark == Mark::Removed => vec![RtSpan::styled(raw.to_owned(), Modifier::DIM)],
            None => vec![RtSpan::raw(raw.to_owned())],
        };
        let number = (mark == Mark::Added).then_some(i + 1);
        out.extend(diff_line_rows(number, mark, spans, layout, ctx));
    }
    if shown < total {
        out.push(RtLine::from(RtSpan::styled(
            format!("⋯ +{} more lines", total - shown),
            style::style_gutter(ctx),
        )));
    }
}

pub(crate) fn display_path_for(path: &Path, cwd: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(cwd) {
        return relative.display().to_string();
    }
    let path_in_same_repo = match (
        nori_config::resolve_root_git_project_for_trust(cwd),
        nori_config::resolve_root_git_project_for_trust(path),
    ) {
        (Some(cwd_repo), Some(path_repo)) => cwd_repo == path_repo,
        _ => false,
    };
    let chosen = if path_in_same_repo {
        pathdiff::diff_paths(path, cwd).unwrap_or_else(|| path.to_path_buf())
    } else {
        relativize_to_home(path)
            .map(|p| PathBuf::from_iter([Path::new("~"), p.as_path()]))
            .unwrap_or_else(|| path.to_path_buf())
    };
    chosen.display().to_string()
}

/// The highlighter's spans for each line of `code`, or `None` when it has no colours for the
/// language (plain text is drawn in the line's own colour).
fn highlighted_diff_lines_for_path(code: &str, path: &Path) -> Option<Vec<Vec<RtSpan<'static>>>> {
    if code.is_empty() {
        return None;
    }

    let lines = highlight_code_to_lines_for_path(code, path);
    let has_syntax_style = lines.iter().flat_map(|line| &line.spans).any(|span| {
        span.style.fg.is_some()
            && span.style.fg != Some(Color::default())
            && span.style.fg != Some(Color::Reset)
    });
    has_syntax_style.then(|| lines.into_iter().map(|line| line.spans).collect())
}
