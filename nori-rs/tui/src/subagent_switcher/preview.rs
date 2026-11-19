//! The actor preview: who the actor is, then the live tail of its conversation.

use nori_tui_components::Preview;
use nori_tui_components::PreviewAnchor;
use nori_tui_components::PreviewRequest;
use nori_tui_components::Theme;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::item::ActorItem;
use super::item::status_glyph;
use super::item::timing_parts;
use super::roster::Body;
use super::roster::Status;
use super::roster::Target;

/// The task, when the title is cut short, shows over at most this many rows.
const TASK_MAX_ROWS: usize = 4;

/// The preview of `item` from the conversation tail `body`, laid out for `request`.
pub(super) fn build(
    request: &PreviewRequest<Target>,
    item: &ActorItem,
    body: &Body,
    tick: i64,
    theme: &Theme,
) -> Preview<Target> {
    let entry = &item.entry;
    let width = usize::from(request.width).max(1);
    let mut facts: Vec<String> = Vec::new();
    facts.extend(entry.agent.clone());
    facts.extend(entry.model.clone());
    facts.extend(
        entry
            .parent
            .as_ref()
            .map(|parent| format!("under {}", parent.label)),
    );
    facts.push(match &entry.detail {
        Some(detail) if entry.status == Status::Failed => format!("failed: {detail}"),
        Some(_) | None => entry.status.word().to_owned(),
    });
    facts.extend(timing_parts(entry, true));
    let title = Line::from(vec![
        status_glyph(entry.status, tick, item.animations, theme),
        " ".into(),
        Span::styled(entry.title.clone(), theme.text_strong),
        Span::styled(format!("  {}", facts.join(" · ")), theme.faint),
    ]);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if UnicodeWidthStr::width(entry.title.as_str()) > width {
        let wrapped = textwrap::wrap(&entry.title, width);
        let more = wrapped.len() > TASK_MAX_ROWS;
        lines.extend(
            wrapped
                .iter()
                .take(TASK_MAX_ROWS)
                .map(|row| Line::from(Span::styled(row.to_string(), theme.text_secondary))),
        );
        if more {
            lines.push(Line::from(Span::styled("…", theme.faint)));
        }
    }
    if body.lines.is_empty() {
        lines.push(Line::from(Span::styled(
            match entry.status {
                Status::Running => "starting…",
                Status::Idle | Status::Completed | Status::Failed | Status::Cancelled => {
                    "nothing yet"
                }
            },
            theme.faint.add_modifier(Modifier::ITALIC),
        )));
    } else {
        lines.extend(body.lines.iter().cloned());
    }
    request.answer(body.revision, title, lines, PreviewAnchor::Tail)
}
