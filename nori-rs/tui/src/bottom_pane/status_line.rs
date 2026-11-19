//! The status line: the one row under the composer.
//!
//! A mode badge, then what the session is running on (agent · model), then, to the right, what
//! it costs the context window and the user: branch, context, cost, and how many actors work.
//! There are no key hints here; the keys are in the `?` overlay. Nothing is repeated: a fact
//! that appears here does not appear in the transcript, and the other way round.
//!
//! The badge is the one loud thing: bold dark text on the mode's own color, so the mode the
//! keyboard is in is never a guess.

use nori_config::FooterSegment;
use nori_tui_components::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::footer::FooterProps;
use crate::bottom_pane::textarea::VimModeState;

/// Context use above this is drawn as a warning.
const CONTEXT_WARN_PERCENT: i64 = 85;
/// Columns between the segments of a group.
const SEPARATOR: &str = "  ·  ";
/// The longest the model label gets before it is cut.
const MODEL_LABEL_MAX_COLUMNS: usize = 44;

/// Something the session wants said for a while, in the status line (not in the transcript, which
/// has its own row for each of these).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StatusNote {
    /// A failed request will be tried again.
    Retry {
        attempt: i64,
        max_attempts: i64,
        at: std::time::Instant,
    },
    /// The session moved to another route (`agent/model`) and runs on it now.
    Route {
        label: String,
        until: std::time::Instant,
    },
    /// The agent kept working after its turn ended.
    Detached,
    /// The user cancelled and the agent went on: Esc again stops the agent itself.
    CancelIgnored,
    /// A prompt is outstanding and the agent has done nothing for `secs` seconds as of `at`; the
    /// minutes tick on until it is heard again or the turn ends.
    AgentSilent { secs: i64, at: std::time::Instant },
    /// Subagents run while the agent is idle: Esc again (before `until`) stops them.
    StopSubagents {
        count: usize,
        until: std::time::Instant,
    },
}

impl StatusNote {
    /// The text now, or `None` once it has run out.
    fn text(&self, now: std::time::Instant) -> Option<String> {
        match self {
            Self::Retry {
                attempt,
                max_attempts,
                at,
            } => {
                let wait = at.saturating_duration_since(now).as_secs();
                Some(format!("retry {attempt}/{max_attempts} in {wait}s"))
            }
            Self::Route { label, until } => (now < *until).then(|| format!("→ {label}")),
            Self::Detached => Some("agent still working".to_owned()),
            Self::CancelIgnored => Some("cancel ignored · Esc stops the agent".to_owned()),
            Self::AgentSilent { secs, at } => {
                let elapsed =
                    i64::try_from(now.saturating_duration_since(*at).as_secs()).unwrap_or_default();
                Some(format!(
                    "agent silent {}m · Esc cancels",
                    (secs + elapsed) / 60
                ))
            }
            Self::StopSubagents { count, until } => (now < *until).then(|| {
                let noun = if *count == 1 { "subagent" } else { "subagents" };
                format!("Esc again stops {count} running {noun} · [ rewinds")
            }),
        }
    }

    /// When the text next changes by itself, so a frame can be drawn then.
    pub(crate) fn next_change(&self, now: std::time::Instant) -> Option<std::time::Instant> {
        match self {
            Self::Retry { at, .. } if *at > now => Some(now + std::time::Duration::from_secs(1)),
            Self::AgentSilent { secs, at } => {
                let elapsed =
                    i64::try_from(now.saturating_duration_since(*at).as_secs()).unwrap_or_default();
                let to_next_minute = 60 - (secs + elapsed) % 60;
                Some(now + std::time::Duration::from_secs(u64::try_from(to_next_minute).ok()?))
            }
            Self::Route { until, .. } | Self::StopSubagents { until, .. } if *until > now => {
                Some(*until)
            }
            Self::Retry { .. }
            | Self::Route { .. }
            | Self::StopSubagents { .. }
            | Self::Detached
            | Self::CancelIgnored => None,
        }
    }

    pub(crate) fn is_over(&self, now: std::time::Instant) -> bool {
        self.text(now).is_none()
    }
}

/// What the keyboard is talking to, as the badge says it.
#[allow(dead_code)] // `Resume` and `Actors` are drawn by the pickers
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Mode {
    Insert,
    Normal,
    /// A prompt is focused for rewinding: which one, of how many.
    Rewind(i64, i64),
    Bash,
    Resume,
    Actors,
    /// The transcript pager.
    Transcript,
    /// Any other full-screen view (a diff, a static pager).
    View,
}

impl Mode {
    pub(crate) fn label(self) -> String {
        match self {
            Self::Insert => "INS".to_owned(),
            Self::Normal => "NOR".to_owned(),
            Self::Rewind(turn, of) => format!("REWIND {turn}/{of}"),
            Self::Bash => "BASH".to_owned(),
            Self::Resume => "RESUME".to_owned(),
            Self::Actors => "ACTORS".to_owned(),
            Self::Transcript => "TRANSCRIPT".to_owned(),
            Self::View => "VIEW".to_owned(),
        }
    }

    fn style(self, theme: &Theme) -> Style {
        match self {
            Self::Insert => theme.badge.insert,
            Self::Normal => theme.badge.normal,
            Self::Rewind(..) => theme.badge.rewind,
            Self::Bash => theme.badge.bash,
            Self::Resume | Self::Transcript => theme.badge.resume,
            Self::Actors | Self::View => theme.badge.actors,
        }
    }
}

/// The badge for `mode`: its label on its color. Pickers use it too, so every mode looks the same
/// wherever it is shown.
pub(crate) fn badge(mode: Mode, theme: &Theme) -> Span<'static> {
    let accent = mode.style(theme);
    let style = match accent.fg {
        Some(color) => Style::new()
            .fg(theme.statusline.bg.unwrap_or(Color::Black))
            .bg(color)
            .add_modifier(Modifier::BOLD),
        None => accent.add_modifier(Modifier::BOLD | Modifier::REVERSED),
    };
    Span::styled(format!(" {} ", mode.label()), style)
}

/// A full-screen view's one status row: just its mode badge on the status surface. Views carry
/// no key hints; `?` in the composer lists the keys.
pub(crate) fn render_mode_row(area: Rect, buf: &mut Buffer, mode: Mode) {
    if area.is_empty() {
        return;
    }
    let theme = crate::style::component_theme();
    let row = Rect { height: 1, ..area };
    buf.set_style(row, theme.statusline);
    Line::from(vec![badge(mode, &theme)])
        .style(theme.statusline)
        .render(row, buf);
}

/// The mode the composer is in right now.
pub(crate) fn mode_of(props: &FooterProps) -> Mode {
    if let Some((turn, of)) = props.rewind_badge {
        Mode::Rewind(turn, of)
    } else if props.bash_mode {
        Mode::Bash
    } else if props.vim_mode_state == Some(VimModeState::Normal) {
        Mode::Normal
    } else {
        Mode::Insert
    }
}

/// Draw the status line into `area` (one row).
pub(crate) fn render(area: Rect, buf: &mut Buffer, props: &FooterProps) {
    if area.is_empty() {
        return;
    }
    let theme = crate::style::component_theme();
    let bar = theme.statusline;
    let row = Rect { height: 1, ..area };
    buf.set_style(row, bar);

    let mut left = vec![badge(mode_of(props), &theme)];
    let enabled = |segment| props.footer_segment_config.is_enabled(segment);
    let right = right_segments(props, &theme, enabled(FooterSegment::GitBranch));
    let right_width = group_width(&right);
    let room = usize::from(area.width)
        .saturating_sub(left[0].width() + 2)
        .saturating_sub(right_width + usize::from(right_width > 0) * 2);
    if let Some(label) = props
        .model_label
        .as_ref()
        .filter(|_| enabled(FooterSegment::Model))
    {
        let label = fit(label, room.min(MODEL_LABEL_MAX_COLUMNS));
        if !label.is_empty() {
            left.push(Span::raw("  "));
            left.push(Span::styled(label, theme.text_secondary));
        }
    }
    if let Some(text) = props
        .status_note
        .as_ref()
        .and_then(|note| note.text(std::time::Instant::now()))
    {
        left.push(Span::raw("  ·  "));
        left.push(Span::styled(text, theme.accent_amber));
    }
    Line::from(left).style(bar).render(row, buf);

    let right = join(right, &theme);
    let used = u16::try_from(right.width()).unwrap_or(area.width);
    let right_area = Rect {
        x: area.right().saturating_sub(used + 1).max(area.x),
        width: used.min(area.width),
        ..row
    };
    right.style(bar).render(right_area, buf);
}

/// The segments of the right group, most important last: when the row is too narrow it sheds from
/// the left.
fn right_segments(
    props: &FooterProps,
    theme: &Theme,
    with_branch: bool,
) -> Vec<Vec<Span<'static>>> {
    let mut groups: Vec<Vec<Span<'static>>> = Vec::new();
    if with_branch && let Some(branch) = props.git_branch.as_ref() {
        let accent = if props.is_worktree {
            theme.accent_coral
        } else {
            theme.accent_violet
        };
        let mut spans = vec![Span::styled(format!("⎇ {branch}"), accent)];
        if let (Some(added), Some(removed)) = (props.git_lines_added, props.git_lines_removed)
            && (added > 0 || removed > 0)
        {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("+{added}"), theme.accent_mint));
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!("−{removed}"), theme.accent_coral));
        }
        groups.push(spans);
    }
    if let Some(cost) = props.agent_cost.as_ref() {
        groups.push(vec![Span::styled(cost.clone(), theme.text_secondary)]);
    }
    if let Some(percent) = props.context_window_percent {
        let percent = percent.clamp(0, 100);
        let style = if percent >= CONTEXT_WARN_PERCENT {
            theme.accent_amber
        } else {
            theme.muted
        };
        groups.push(vec![Span::styled(format!("{percent}%"), style)]);
    }
    if props.active_subagent_count > 0 {
        groups.push(vec![Span::styled(
            format!("● {}", props.active_subagent_count),
            theme.accent_cyan,
        )]);
    }
    if props.monitor_count > 0 {
        let noun = if props.monitor_count == 1 {
            "monitor"
        } else {
            "monitors"
        };
        groups.push(vec![Span::styled(
            format!("◉ {} {noun}", props.monitor_count),
            theme.accent_mint,
        )]);
    }
    groups
}

fn group_width(groups: &[Vec<Span<'static>>]) -> usize {
    let spans: usize = groups
        .iter()
        .map(|group| group.iter().map(Span::width).sum::<usize>())
        .sum();
    spans + SEPARATOR.width() * groups.len().saturating_sub(1)
}

fn join(groups: Vec<Vec<Span<'static>>>, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::new();
    for (index, group) in groups.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(SEPARATOR, theme.faint));
        }
        spans.extend(group);
    }
    Line::from(spans)
}

/// `text` cut to `columns` columns, with an ellipsis when it was cut.
fn fit(text: &str, columns: usize) -> String {
    if text.width() <= columns {
        return text.to_owned();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width + 1 > columns {
            break;
        }
        out.push(ch);
        used += width;
    }
    if out.is_empty() {
        out
    } else {
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn labels_are_short_and_say_where_a_rewind_is() {
        assert_eq!(Mode::Insert.label(), "INS");
        assert_eq!(Mode::Rewind(2, 3).label(), "REWIND 2/3");
    }

    #[test]
    fn a_long_label_is_cut_with_an_ellipsis() {
        assert_eq!(fit("Mock ACP · Mock Default Model", 12), "Mock ACP · …");
        assert_eq!(fit("short", 12), "short");
    }
}
