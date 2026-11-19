//! The stacked per-provider chart over time.

use std::collections::BTreeSet;

use chrono::Datelike;
use chrono::Timelike;
use nori_harness::usage::TimeBucket;
use nori_tui_components::charts::Bar;
use nori_tui_components::charts::stacked_bar_rows;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::color_of;
use super::format::pad_left;
use super::format::provider_label;
use super::format::provider_style;
use super::view::View;

const CHART_HEIGHT: i32 = 6;
const AXIS_WIDTH: i32 = 7;

pub(super) fn chart(view: &View) -> Vec<Line<'static>> {
    let theme = view.theme;
    let buckets = &view.summary.buckets;
    // Only providers with something to draw: a provider with no cost in the period has no bar.
    let providers: BTreeSet<&str> = buckets
        .iter()
        .flat_map(|bucket| bucket.providers.iter())
        .filter(|(_, value)| view.measure(value.cost_usd, value.tokens) > 0.0)
        .map(|(provider, _)| provider.as_str())
        .collect();
    let bars: Vec<Bar> = buckets
        .iter()
        .map(|bucket| {
            providers
                .iter()
                .filter_map(|provider| {
                    let value = bucket.providers.get(*provider)?;
                    Some((
                        view.measure(value.cost_usd, value.tokens),
                        color_of(provider_style(theme, provider)),
                    ))
                })
                .collect()
        })
        .collect();
    let title = if view.summary.period.is_hourly() {
        "Hourly"
    } else {
        "Daily"
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(title.to_owned(), theme.text_strong),
        Span::styled(format!(" {}", view.metric.label()), theme.muted),
    ])];
    lines.extend(plot(view, &bars, CHART_HEIGHT));
    if !providers.is_empty() {
        let mut legend = vec![Span::raw(
            " ".repeat(usize::try_from(AXIS_WIDTH).unwrap_or(0)),
        )];
        for (index, provider) in providers.iter().enumerate() {
            if index > 0 {
                legend.push(Span::raw("  "));
            }
            legend.push(Span::styled("■ ", provider_style(theme, provider)));
            legend.push(Span::styled(provider_label(provider), theme.muted));
        }
        lines.push(Line::from(legend));
    }
    lines
}

/// Stacked `bars` (one per bucket of the summary) with a value axis on the left and hour or day
/// labels underneath.
pub(super) fn plot(view: &View, bars: &[Bar], height: i32) -> Vec<Line<'static>> {
    let theme = view.theme;
    let buckets = &view.summary.buckets;
    let count = i32::try_from(buckets.len()).unwrap_or(1).max(1);
    let room = (view.width - AXIS_WIDTH - 1).max(count);
    let slot = (room / count).clamp(1, 6);
    let (bar_width, gap) = if slot >= 2 { (slot - 1, 1) } else { (1, 0) };
    let peak = bars
        .iter()
        .map(|bar| bar.iter().map(|(value, _)| value).sum::<f64>())
        .fold(0.0, f64::max);

    let rows = stacked_bar_rows(bars, peak, height, bar_width, gap);
    let top = view.show_axis(peak);
    let middle = view.show_axis(peak / 2.0);
    let last_row = rows.len().saturating_sub(1);
    let mut lines = Vec::new();
    for (index, row) in rows.into_iter().enumerate() {
        let label = if index == 0 {
            top.clone()
        } else if index == last_row / 2 && last_row > 1 {
            middle.clone()
        } else if index == last_row {
            "0".to_owned()
        } else {
            String::new()
        };
        let mut spans = vec![
            Span::styled(pad_left(&label, AXIS_WIDTH - 1), theme.faint),
            Span::styled("▕", theme.faint),
        ];
        spans.extend(row.spans);
        lines.push(Line::from(spans));
    }
    lines.push(axis_labels(buckets, view, bar_width + gap));
    lines
}

impl View<'_> {
    fn show_axis(&self, value: f64) -> String {
        match self.metric {
            super::view::Metric::Cost => super::format::format_usd(value),
            super::view::Metric::Tokens => super::format::format_tokens(value as i64),
        }
    }
}

/// Hour or day labels under the bars, as many as fit without touching.
fn axis_labels(buckets: &[TimeBucket], view: &View, slot: i32) -> Line<'static> {
    let hourly = view.summary.period.is_hourly();
    let slot = usize::try_from(slot).unwrap_or(1).max(1);
    let label_width: usize = if hourly || buckets.len() > 7 { 2 } else { 3 };
    let every = (label_width + 1).div_ceil(slot).max(1);
    let mut text = " ".repeat(usize::try_from(AXIS_WIDTH).unwrap_or(0));
    let mut cursor = 0;
    for (index, bucket) in buckets
        .iter()
        .enumerate()
        .rev()
        .step_by(every)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let label = if hourly {
            format!("{:02}", bucket.start.hour())
        } else if buckets.len() <= 7 {
            bucket.start.format("%a").to_string()
        } else {
            format!("{}", bucket.start.day())
        };
        let at = index * slot;
        if at < cursor {
            continue;
        }
        text.push_str(&" ".repeat(at - cursor));
        text.push_str(&label);
        cursor = at + label.chars().count();
    }
    Line::from(Span::styled(text, view.theme.faint))
}
