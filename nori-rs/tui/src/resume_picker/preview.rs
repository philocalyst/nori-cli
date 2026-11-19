//! Laying out a session's digest as the preview pane: user turns on a rail, assistant prose,
//! tool calls with their commands highlighted. Runs on a blocking task, never on the UI thread.

use nori_harness::session_index::DigestTurn;
use nori_harness::session_index::SessionDigest;
use nori_harness::session_index::TurnRole;
use nori_tui_components::Preview;
use nori_tui_components::PreviewAnchor;
use nori_tui_components::PreviewRequest;
use nori_tui_components::Theme;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;

use super::helix_entry::ResumeEntry;
use crate::helix_picker::clip;
use crate::render::bash_highlight::highlight_bash_spans;

/// Turns laid out at most; a longer conversation shows its first turns and says so.
const MAX_TURNS: usize = 2000;
/// Lines of a tool's output shown under it.
const OUTPUT_PREVIEW_LINES: usize = 3;

pub(super) fn title_line(entry: &ResumeEntry, theme: &Theme) -> Line<'static> {
    let mut parts: Vec<String> = Vec::new();
    // The directory is news only when it is not the one Nori runs in.
    let here = std::env::current_dir().ok();
    parts.extend(
        entry
            .cwd
            .as_deref()
            .filter(|cwd| here.as_deref() != Some(std::path::Path::new(cwd)))
            .map(shorten_home),
    );
    parts.extend(entry.agent.clone());
    parts.extend(entry.model.clone());
    Line::from(Span::styled(parts.join(" · "), theme.text_secondary))
}

fn shorten_home(path: &str) -> String {
    match dirs::home_dir() {
        Some(home) => match path.strip_prefix(home.to_string_lossy().as_ref()) {
            Some(rest) => format!("~{rest}"),
            None => path.to_owned(),
        },
        None => path.to_owned(),
    }
}

/// The preview of a digest, wrapped to the request's width.
pub(super) fn from_digest(
    request: &PreviewRequest<i64>,
    entry: &ResumeEntry,
    digest: Option<&SessionDigest>,
    theme: &Theme,
) -> Preview<i64> {
    let width = usize::from(request.width).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match digest {
        Some(digest) if !digest.turns.is_empty() => {
            for turn in digest.turns.iter().take(MAX_TURNS) {
                push_turn(&mut lines, turn, width, theme);
            }
            if digest.turns.len() > MAX_TURNS {
                lines.push(Line::from(Span::styled(
                    format!("… {} more turns", digest.turns.len() - MAX_TURNS),
                    theme.faint,
                )));
            }
        }
        Some(_) => lines.push(note("No conversation recorded in this session.", theme)),
        None => lines.push(note(
            "Indexing this session… its conversation appears here in a moment.",
            theme,
        )),
    }
    request.answer(
        entry.revision,
        title_line(entry, theme),
        lines,
        PreviewAnchor::FirstMatch,
    )
}

/// The preview of an entry with no recorded transcript (a session an agent listed, or the
/// "start a new session" row): what is known about it.
pub(super) fn from_entry(
    request: &PreviewRequest<i64>,
    entry: &ResumeEntry,
    theme: &Theme,
) -> Preview<i64> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if matches!(entry.action, super::helix_entry::ResumeAction::StartFresh) {
        lines.push(note("Start a fresh conversation in this directory.", theme));
    } else {
        let fact = |label: &str, value: &str| {
            Line::from(vec![
                Span::styled(format!("{label:<10}"), theme.faint),
                Span::styled(value.to_owned(), theme.text),
            ])
        };
        lines.push(Line::from(Span::styled(
            entry.title.clone(),
            theme.text_strong,
        )));
        lines.push(Line::default());
        if let Some(cwd) = &entry.cwd {
            lines.push(fact("Directory", &shorten_home(cwd)));
        }
        if !entry.started_at.is_empty() {
            lines.push(fact("Updated", &entry.started_at));
        }
        lines.push(fact("Session", &entry.session_id));
    }
    request.answer(
        entry.revision,
        title_line(entry, theme),
        lines,
        PreviewAnchor::Top,
    )
}

fn note(text: &str, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        theme.faint.add_modifier(Modifier::ITALIC),
    ))
}

fn push_turn(lines: &mut Vec<Line<'static>>, turn: &DigestTurn, width: usize, theme: &Theme) {
    match turn.role {
        TurnRole::User => {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            for (index, row) in textwrap::wrap(&turn.text, width.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                let rail = if index == 0 { "▌ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(rail, theme.accent_violet),
                    Span::styled(row.into_owned(), theme.text_strong),
                ]));
            }
            lines.push(Line::default());
        }
        TurnRole::Assistant => {
            for row in textwrap::wrap(&turn.text, width) {
                lines.push(Line::from(Span::styled(row.into_owned(), theme.text)));
            }
            lines.push(Line::default());
        }
        TurnRole::Tool => {
            match &turn.command {
                Some(command) => {
                    let mut spans: Vec<Span<'static>> = vec![Span::styled("$ ", theme.accent_mint)];
                    spans.extend(highlight_bash_spans(&clip(
                        command,
                        width.saturating_sub(2),
                    )));
                    lines.push(Line::from(spans));
                }
                None => lines.push(Line::from(vec![
                    Span::styled("• ", theme.faint),
                    Span::styled(clip(&turn.text, width.saturating_sub(2)), theme.faint),
                ])),
            }
            // The first lines of what it printed, so a search for an error message shows it.
            for row in turn
                .output
                .iter()
                .flat_map(|output| output.lines())
                .filter(|row| !row.trim().is_empty())
                .take(OUTPUT_PREVIEW_LINES)
            {
                lines.push(Line::from(Span::styled(
                    format!("  {}", clip(row, width.saturating_sub(2))),
                    theme.faint,
                )));
            }
        }
    }
}
