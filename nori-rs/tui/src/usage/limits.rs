//! Rate-limit windows, per provider and then per account: a provider's accounts never pool, and
//! their windows sit side by side so where the room is reads at a glance.

use chrono::DateTime;
use chrono::Utc;
use nori_harness::usage::AccountLimits;
use nori_harness::usage::PooledWindow;
use nori_protocol::RateLimitKind;
use nori_tui_components::charts::meter_spans;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::clip;
use super::format::format_countdown;
use super::format::pad_left;
use super::format::pad_right;
use super::format::provider_label;
use super::format::provider_style;
use super::view::View;

const METER_WIDTH: i32 = 20;
const LABEL_WIDTH: i32 = 7;
/// Everything in a side-by-side window cell but its meter: label, gaps, percent, countdown.
const CELL_FIXED: i32 = 4 + 1 + 1 + 4 + 1 + 8;
const MIN_CELL_METER: i32 = 5;
/// The longest account name an account row spells out.
const ACCOUNT_WIDTH: i32 = 16;
/// How many points over the share of the window that has passed counts as ahead of pace.
const AHEAD_OF_PACE: f64 = 10.0;
/// How many points of room one account needs over its sibling to be called out as having most.
const ROOM_GAP: f64 = 10.0;

/// One window shared by the accounts of a provider, as a column of the group.
struct Column {
    id: String,
    label: String,
    kind: RateLimitKind,
}

fn kind_rank(kind: RateLimitKind) -> i32 {
    match kind {
        RateLimitKind::Session => 0,
        RateLimitKind::Weekly => 1,
        RateLimitKind::Monthly => 2,
        RateLimitKind::Other => 3,
    }
}

fn columns(group: &[&AccountLimits]) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();
    for pooled in group.iter().flat_map(|account| &account.windows) {
        if !columns.iter().any(|column| column.id == pooled.window.id) {
            columns.push(Column {
                id: pooled.window.id.clone(),
                label: pooled.window.label.clone(),
                kind: pooled.window.kind,
            });
        }
    }
    columns.sort_by_key(|column| kind_rank(column.kind));
    columns
}

pub(super) fn limits(view: &View) -> Vec<Line<'static>> {
    let mut lines = vec![view.section_title("Limits")];
    if view.summary.limits.is_empty() {
        // The sentence wraps to the room there is instead of running off the edge.
        let room = usize::try_from(view.width.saturating_sub(4))
            .unwrap_or(20)
            .max(20);
        for row in textwrap::wrap(
            "No quota reported yet. Agents that expose rate limits show their windows here.",
            room,
        ) {
            lines.push(Line::from(Span::styled(row.into_owned(), view.theme.faint)));
        }
        return lines;
    }
    lines.extend(tightest(view));
    let mut rest: &[AccountLimits] = &view.summary.limits;
    while let Some(first) = rest.first() {
        let count = rest
            .iter()
            .take_while(|account| account.provider() == first.provider())
            .count();
        let (group, tail) = rest.split_at(count);
        rest = tail;
        lines.push(Line::default());
        lines.extend(provider_group(view, &group.iter().collect::<Vec<_>>()));
    }
    lines
}

/// The one window that leaves the least room among the accounts in use, and where else the
/// same provider has room: the line a status line or a hover would carry.
fn tightest(view: &View) -> Option<Line<'static>> {
    let theme = view.theme;
    let constraint = view.summary.most_constrained()?;
    let used = constraint.window.used_percent();
    let fill = used_style(view, used);
    let mut spans = vec![
        Span::styled("Tightest  ", theme.faint),
        Span::styled(provider_label(&constraint.account.provider), theme.text),
        Span::styled(format!(" {}", constraint.account.describe()), theme.faint),
        Span::styled(
            format!("  {} {used:.0}% used", constraint.window.window.label),
            fill,
        ),
    ];
    // Each piece is added only if the row still fits, most useful first.
    let mut pieces = Vec::new();
    if let Some((account, room)) = constraint.spare {
        pieces.push((
            format!(" · {} has {room:.0}% left", account.describe()),
            theme.accent_mint,
        ));
    }
    if let Some(reset) = constraint
        .window
        .window
        .resets_at
        .as_deref()
        .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
        .filter(|_| !constraint.window.refilled)
    {
        pieces.push((
            format!(
                " · resets in {}",
                format_countdown(reset.with_timezone(&Utc), view.now)
            ),
            theme.muted,
        ));
    }
    let mut width: usize = spans.iter().map(Span::width).sum();
    for (text, style) in pieces {
        width += text.chars().count();
        if i32::try_from(width).unwrap_or(i32::MAX) > view.width {
            break;
        }
        spans.push(Span::styled(text, style));
    }
    Some(Line::from(spans))
}

fn provider_group(view: &View, group: &[&AccountLimits]) -> Vec<Line<'static>> {
    let theme = view.theme;
    let provider = group[0].provider();
    let columns = columns(group);
    let mut lines = vec![group_header(view, provider, group, &columns)];

    let labels: Vec<String> = group
        .iter()
        .map(|account| clip(&account.account.describe(), ACCOUNT_WIDTH))
        .collect();
    let label_width = labels
        .iter()
        .map(|label| i32::try_from(label.chars().count()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    let columns_count = i32::try_from(columns.len().max(1)).unwrap_or(1);
    let room = view.width - 4 - label_width - 2;
    let meter = (room / columns_count - 2 - CELL_FIXED).min(METER_WIDTH);
    let side_by_side = meter >= MIN_CELL_METER;
    let best = best_room(group);
    for (index, (account, label)) in group.iter().zip(&labels).enumerate() {
        let name = Span::styled(
            pad_right(label, label_width),
            if best == Some(index) {
                theme.accent_mint
            } else {
                theme.text_secondary
            },
        );
        if side_by_side {
            let mut spans = vec![Span::raw("    "), name, Span::raw("  ")];
            for (position, column) in columns.iter().enumerate() {
                if position > 0 {
                    spans.push(Span::raw("  "));
                }
                let pooled = account
                    .windows
                    .iter()
                    .find(|pooled| pooled.window.id == column.id);
                spans.extend(window_cell(view, column, pooled, meter));
            }
            if best == Some(index) {
                let used: usize = spans.iter().map(Span::width).sum();
                if i32::try_from(used + 11).unwrap_or(i32::MAX) <= view.width {
                    spans.push(Span::styled("  most room", theme.accent_mint));
                }
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![Span::raw("    "), name]));
            lines.extend(
                account
                    .windows
                    .iter()
                    .map(|pooled| window_line(view, pooled)),
            );
        }
    }
    lines
}

/// `● Claude · 2 accounts · 41% / 12% of 5h used`, or just the provider when it has one account.
fn group_header(
    view: &View,
    provider: &str,
    group: &[&AccountLimits],
    columns: &[Column],
) -> Line<'static> {
    let theme = view.theme;
    let mut spans = vec![
        Span::styled("● ", provider_style(theme, provider)),
        Span::styled(provider_label(provider), theme.text),
    ];
    if group.len() == 1 {
        // A lone account is named only when its name says something: an email, a gateway.
        let account = &group[0].account;
        if !account.key.starts_with("agent:") {
            spans.push(Span::styled(
                format!("  {}", clip(&account.describe(), view.width / 2)),
                theme.faint,
            ));
        }
        return Line::from(spans);
    }
    spans.push(Span::styled(
        format!(" · {} accounts", group.len()),
        theme.muted,
    ));
    let Some(first) = columns.first() else {
        return Line::from(spans);
    };
    let usage: Vec<Option<f64>> = group
        .iter()
        .map(|account| {
            account
                .windows
                .iter()
                .find(|pooled| pooled.window.id == first.id)
                .map(PooledWindow::used_percent)
        })
        .collect();
    spans.push(Span::styled(" · ", theme.faint));
    for (index, used) in usage.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(" / ", theme.faint));
        }
        match used {
            Some(used) => spans.push(Span::styled(format!("{used:.0}%"), used_style(view, *used))),
            None => spans.push(Span::styled("–", theme.faint)),
        }
    }
    spans.push(Span::styled(
        format!(" of {} used", first.label),
        theme.muted,
    ));
    Line::from(spans)
}

/// The account with clearly the most room, when there are several and they differ.
fn best_room(group: &[&AccountLimits]) -> Option<usize> {
    if group.len() < 2 {
        return None;
    }
    let rooms: Vec<Option<f64>> = group.iter().map(|account| account.headroom()).collect();
    let (index, best) = rooms
        .iter()
        .enumerate()
        .filter_map(|(index, room)| Some((index, (*room)?)))
        .max_by(|a, b| a.1.total_cmp(&b.1))?;
    let runner_up = rooms
        .iter()
        .enumerate()
        .filter(|(other, _)| *other != index)
        .filter_map(|(_, room)| *room)
        .fold(f64::NEG_INFINITY, f64::max);
    (best - runner_up >= ROOM_GAP).then_some(index)
}

/// `5h ▕████░░░░▏ 41% 1h 12m`, or blank cells when the account has no such window.
fn window_cell(
    view: &View,
    column: &Column,
    pooled: Option<&PooledWindow>,
    meter: i32,
) -> Vec<Span<'static>> {
    let theme = view.theme;
    let cell = |text: String, style: Style| Span::styled(text, style);
    let label = cell(pad_right(&column.label, 4), theme.text_secondary);
    let Some(pooled) = pooled else {
        let blank = 4 + 1 + meter + 1 + 4 + 1 + 8;
        return vec![
            label,
            cell(pad_right(" none reported", blank - 4), theme.faint),
        ];
    };
    let used = pooled.used_percent();
    let fill = used_style(view, used);
    let pace = pace_percent(pooled, view.now).map(|elapsed| (elapsed, theme.muted));
    let mut spans = vec![label, Span::raw(" ")];
    spans.extend(meter_spans(used, meter, fill, theme.faint, pace));
    spans.push(cell(pad_left(&format!("{used:.0}%"), 5), fill));
    let when = if pooled.refilled {
        "refilled".to_owned()
    } else {
        pooled
            .window
            .resets_at
            .as_deref()
            .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
            .map_or_else(String::new, |time| {
                format_countdown(time.with_timezone(&Utc), view.now)
            })
    };
    spans.push(cell(format!(" {}", pad_right(&when, 8)), theme.muted));
    spans
}

/// Meter color by how close the window is to empty.
fn used_style(view: &View, percent: f64) -> Style {
    if percent >= 90.0 {
        view.theme.accent_coral
    } else if percent >= 75.0 {
        view.theme.accent_amber
    } else {
        view.theme.accent_mint
    }
}

fn pace_percent(window: &PooledWindow, now: DateTime<Utc>) -> Option<f64> {
    let length = window
        .window
        .window_minutes
        .filter(|minutes| *minutes > 0)?;
    let reset = DateTime::parse_from_rfc3339(window.window.resets_at.as_deref()?).ok()?;
    let remaining = (reset.with_timezone(&Utc) - now).num_minutes();
    Some((100.0 - remaining as f64 / length as f64 * 100.0).clamp(0.0, 100.0))
}

/// One window on a row of its own: the layout for rooms too narrow for columns.
fn window_line(view: &View, pooled: &PooledWindow) -> Line<'static> {
    let theme = view.theme;
    let window = &pooled.window;
    let used = window.used_percent;
    let fill = used_style(view, used);
    let elapsed = pace_percent(pooled, view.now);
    let pace = elapsed.map(|elapsed| (elapsed, theme.muted));
    let meter = (view.width - 4 - LABEL_WIDTH - 24).clamp(6, METER_WIDTH);
    let mut spans = vec![
        Span::raw("      "),
        Span::styled(pad_right(&window.label, LABEL_WIDTH), theme.text_secondary),
    ];
    spans.extend(meter_spans(used, meter, fill, theme.faint, pace));
    spans.push(Span::styled(pad_left(&format!("{used:.0}%"), 5), fill));
    if pooled.refilled {
        // Its reset time passed: whatever was used is gone, and nothing has been reported since.
        spans.push(Span::styled(" used · idle", theme.muted));
        return Line::from(spans);
    }
    let reset = window
        .resets_at
        .as_deref()
        .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
        .map(|time| time.with_timezone(&Utc));
    // Each piece is added only if the row still fits, most important first.
    let mut pieces = vec![
        (" used".to_owned(), fill),
        (format!(" · {:.0}% left", 100.0 - used), theme.muted),
    ];
    match reset {
        Some(reset) => pieces.push((
            format!(" · resets in {}", format_countdown(reset, view.now)),
            theme.muted,
        )),
        // Nothing used and nothing to wait for.
        None if used < 0.5 => pieces.push((" · idle".to_owned(), theme.muted)),
        None => {}
    }
    if elapsed.is_some_and(|elapsed| used > elapsed + AHEAD_OF_PACE) {
        pieces.push(("   ahead of pace".to_owned(), theme.accent_amber));
    }
    if pooled.sessions > 1 {
        pieces.push((
            format!("   pooled · {} sessions", pooled.sessions),
            theme.faint,
        ));
    }
    let mut used_width: usize = spans.iter().map(Span::width).sum();
    for (text, style) in pieces {
        used_width += text.chars().count();
        if i32::try_from(used_width).unwrap_or(i32::MAX) > view.width {
            break;
        }
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}
