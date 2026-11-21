//! A row of the actor panel: a session in the tree, searchable, drawn on one line.

use std::time::Duration;

use nori_tui_components::Field;
use nori_tui_components::HelixItem;
use nori_tui_components::RowContext;
use nori_tui_components::fit_snippet;
use nori_tui_components::highlight_indices;
use nori_tui_components::highlight_terms;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::roster::Entry;
use super::roster::Status;
use super::roster::Target;
use crate::chatwidget::subagents::SPINNER_FRAMES;
use crate::chatwidget::subagents::format_elapsed;
use crate::chatwidget::subagents::tool_count_text;
use crate::helix_picker::clip;

/// The longest the name of a parent is allowed to be in `parent › child`.
const PARENT_MAX_WIDTH: usize = 16;

/// An actor and where it sits in the tree.
#[derive(Debug, Clone)]
pub(crate) struct ActorItem {
    pub(crate) entry: Entry,
    /// Tree guides drawn before the status glyph: `├─ `, `│  └─ `. Empty for the root.
    pub(crate) guides: String,
    pub(crate) animations: bool,
}

/// The glyph for a status: the spinner while running, an open circle at rest, a check, a cross, a
/// slashed circle.
pub(crate) fn status_glyph(
    status: Status,
    tick: i64,
    animations: bool,
    theme: &nori_tui_components::Theme,
) -> Span<'static> {
    match status {
        Status::Running if animations => {
            let frame = usize::try_from(tick.rem_euclid(10)).unwrap_or(0);
            Span::styled(SPINNER_FRAMES[frame], theme.accent_cyan)
        }
        Status::Running => Span::styled("●", theme.accent_cyan),
        Status::Idle => Span::styled("○", theme.faint),
        Status::Completed => Span::styled("✓", theme.accent_mint),
        Status::Failed => Span::styled("✗", theme.accent_coral),
        Status::Cancelled => Span::styled("⊘", theme.faint),
    }
}

/// How long ago, in the coarsest unit that is not zero: `just now`, `12s ago`, `2m ago`, `3h ago`.
pub(crate) fn ago(elapsed: Duration) -> String {
    match elapsed.as_secs() {
        0..=4 => "just now".to_owned(),
        seconds @ 5..=59 => format!("{seconds}s ago"),
        seconds @ 60..=3599 => format!("{}m ago", seconds / 60),
        seconds => format!("{}h ago", seconds / 3600),
    }
}

/// Tool count, then how long it ran and, once ended, how long ago. A row passes
/// `ended_duration: false`: an ended actor reads `· 28s ago` there, and its duration is in the
/// preview header.
pub(crate) fn timing_parts(entry: &Entry, ended_duration: bool) -> Vec<String> {
    let mut parts = Vec::new();
    if entry.tool_count > 0 {
        parts.push(tool_count_text(entry.tool_count));
    }
    // A duration of nothing is not shown.
    let ran = (entry.elapsed >= Duration::from_secs(1)).then(|| format_elapsed(entry.elapsed));
    match (entry.status, entry.ended_ago) {
        (Status::Idle, _) => {}
        (Status::Running, _) | (Status::Completed | Status::Failed | Status::Cancelled, None) => {
            parts.extend(ran);
        }
        (Status::Completed | Status::Failed | Status::Cancelled, Some(ended)) => {
            if ended_duration {
                parts.extend(ran);
            }
            parts.push(ago(ended));
        }
    }
    parts
}

impl HelixItem for ActorItem {
    type Id = Target;

    fn id(&self) -> Target {
        self.entry.target
    }

    fn fields<'a>(&'a self, out: &mut Vec<Field<'a>>) {
        let entry = &self.entry;
        out.push(Field::new("title", &entry.title, 0));
        if let Some(agent) = &entry.agent {
            out.push(Field::new("agent", agent, 30));
        }
        if let Some(model) = &entry.model {
            out.push(Field::new("model", model, 40));
        }
        if let Some(parent) = &entry.parent {
            out.push(Field::new("parent", &parent.label, 50));
        }
        if let Some(detail) = &entry.detail {
            out.push(Field::new("detail", detail, 55));
        }
        out.push(Field::new("status", entry.status.word(), 60));
        if entry.status == Status::Completed {
            out.push(Field::new("status", "completed", 60));
        }
    }

    fn body(&self) -> Option<&str> {
        Some(self.entry.transcript_text.as_str()).filter(|text| !text.is_empty())
    }

    fn revision(&self) -> i64 {
        self.entry.revision
    }

    fn is_live(&self) -> bool {
        self.entry.status.is_running()
    }

    fn search_identity_eq(&self, other: &Self) -> bool {
        self.entry.same_identity(&other.entry)
    }

    fn rows(&self, cx: &RowContext<'_>) -> Vec<Line<'static>> {
        let entry = &self.entry;
        let width = usize::from(cx.width);
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut used = 0;
        if !cx.filtered && !self.guides.is_empty() {
            spans.push(Span::styled(self.guides.clone(), cx.theme.faint));
            used += UnicodeWidthStr::width(self.guides.as_str());
        }
        let glyph = match entry.target {
            Target::Main => Span::styled("◈", cx.theme.accent_violet),
            Target::Subagent(_) => status_glyph(entry.status, cx.tick, self.animations, cx.theme),
        };
        spans.push(glyph);
        spans.push(" ".into());
        used += 2;
        if cx.filtered
            && let Some(parent) = &entry.parent
        {
            let label = clip(&parent.label, PARENT_MAX_WIDTH);
            used += UnicodeWidthStr::width(label.as_str()) + 3;
            spans.push(Span::styled(label, cx.theme.faint));
            spans.push(Span::styled(" › ", cx.theme.faint));
        }
        let base = if cx.selected {
            cx.theme.text_strong
        } else {
            Style::new()
        };
        let (title, title_width) = highlight_indices(
            &entry.title,
            cx.highlights.field("title"),
            base,
            cx.theme.accent_amber,
            width.saturating_sub(used),
        );
        spans.extend(title);
        used += title_width;
        if title_width < UnicodeWidthStr::width(entry.title.as_str()) {
            return vec![Line::from(spans)];
        }
        let details = match cx.hit.and_then(|hit| hit.snippet.as_deref()) {
            Some(snippet) => {
                highlight_terms(
                    Line::from(Span::styled(
                        fit_snippet(snippet, cx.terms, width.saturating_sub(used + 2)),
                        cx.theme.faint.add_modifier(Modifier::ITALIC),
                    )),
                    cx.terms,
                    cx.theme.accent_amber,
                )
                .spans
            }
            None => details(entry, cx, width.saturating_sub(used + 2)),
        };
        if !details.is_empty() {
            spans.push("  ".into());
            spans.extend(details);
        }
        vec![Line::from(spans)]
    }
}

/// Agent, model, tool count and time, as many as fit in `room` columns. When they do not all
/// fit, the time and the tool count win over the agent and the model.
fn details(entry: &Entry, cx: &RowContext<'_>, room: usize) -> Vec<Span<'static>> {
    let faint = cx.theme.faint;
    // In display order, each with its priority: lower is kept first.
    let mut parts: Vec<(i32, String)> = Vec::new();
    parts.extend(entry.agent.clone().map(|agent| (2, agent)));
    parts.extend(entry.model.clone().map(|model| (3, model)));
    let timing = timing_parts(entry, false);
    let last = timing.len().saturating_sub(1);
    parts.extend(
        timing
            .into_iter()
            .enumerate()
            .map(|(index, text)| (i32::from(index != last), text)),
    );
    let mut order: Vec<usize> = (0..parts.len()).collect();
    order.sort_by_key(|index| parts[*index].0);
    let mut kept = vec![false; parts.len()];
    let mut taken = 0;
    let mut count = 0;
    for index in order {
        let separator = if count == 0 { 0 } else { 3 };
        let width = UnicodeWidthStr::width(parts[index].1.as_str());
        if taken + separator + width <= room {
            kept[index] = true;
            taken += separator + width;
            count += 1;
        }
    }
    let mut spans = Vec::new();
    for (index, (_, text)) in parts.into_iter().enumerate() {
        if !kept[index] {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::styled(" · ", faint));
        }
        spans.push(Span::styled(text, faint));
    }
    spans
}
