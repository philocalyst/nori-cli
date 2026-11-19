//! The headline figure and the per-provider rows beside it.

use nori_harness::usage::ProviderUsage;
use nori_tui_components::charts::sparkline;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::format_share;
use super::format::format_tokens;
use super::format::format_usd;
use super::format::pad_left;
use super::format::pad_right;
use super::format::provider_label;
use super::format::provider_style;
use super::view::Metric;
use super::view::View;

pub(super) fn overview(view: &View) -> Vec<Line<'static>> {
    let theme = view.theme;
    let totals = &view.summary.totals;
    let figure = view.show(totals.cost_usd, totals.tokens.total());
    let other = match view.metric {
        Metric::Cost => format!("{} tokens", format_tokens(totals.tokens.total())),
        Metric::Tokens if totals.cost_usd > 0.0 => {
            format!("{} at API prices", format_usd(totals.cost_usd))
        }
        Metric::Tokens => "no prices known".to_owned(),
    };
    let sessions = format!(
        "{} session{}",
        totals.sessions,
        if totals.sessions == 1 { "" } else { "s" }
    );
    let mut note = String::new();
    if view.metric == Metric::Cost {
        if totals.unpriced_tokens > 0 {
            note = format!("{} tokens unpriced", format_tokens(totals.unpriced_tokens));
        } else if totals.estimated_cost_usd > 0.0 {
            note = format!("{} estimated", format_usd(totals.estimated_cost_usd));
        }
    }
    let headline = vec![
        Line::from(Span::styled(
            figure,
            theme
                .text_strong
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(sessions, theme.text_secondary),
            Span::styled(" · ", theme.faint),
            Span::styled(other, theme.muted),
        ]),
        Line::from(Span::styled(note, theme.faint)),
    ];

    // The headline column is as wide as its widest line, and the provider rows sit beside it
    // when there is room for them.
    let headline_width = i32::try_from(
        headline
            .iter()
            .map(|line| line.spans.iter().map(Span::width).sum::<usize>())
            .max()
            .unwrap_or(0),
    )
    .unwrap_or(0)
        + 4;
    let providers = provider_rows(view, headline_width);
    if view.width < headline_width + 44 {
        return headline.into_iter().chain(providers).collect();
    }
    let rows = headline.len().max(providers.len());
    (0..rows)
        .map(|row| {
            let mut spans = match headline.get(row) {
                Some(line) => {
                    let text_width: usize = line.spans.iter().map(Span::width).sum();
                    let pad = usize::try_from(headline_width)
                        .unwrap_or(0)
                        .saturating_sub(text_width);
                    let mut spans = line.spans.clone();
                    spans.push(Span::raw(" ".repeat(pad)));
                    spans
                }
                None => vec![Span::raw(
                    " ".repeat(usize::try_from(headline_width).unwrap_or(0)),
                )],
            };
            if let Some(line) = providers.get(row) {
                spans.extend(line.spans.clone());
            }
            Line::from(spans)
        })
        .collect()
}

fn provider_rows(view: &View, headline_width: i32) -> Vec<Line<'static>> {
    let theme = view.theme;
    let mut providers: Vec<&ProviderUsage> = view.summary.providers.iter().collect();
    providers.sort_by(|a, b| {
        view.measure(b.cost_usd, b.tokens)
            .total_cmp(&view.measure(a.cost_usd, a.tokens))
    });
    let total: f64 = providers
        .iter()
        .map(|provider| view.measure(provider.cost_usd, provider.tokens))
        .sum();
    let trend_width = (view.width - headline_width - 40).clamp(0, 30);
    providers
        .into_iter()
        .take(6)
        .map(|provider| {
            let style = provider_style(theme, &provider.provider);
            let measured = view.measure(provider.cost_usd, provider.tokens);
            let share = if total > 0.0 { measured / total } else { 0.0 };
            let series: Vec<f64> = view
                .summary
                .buckets
                .iter()
                .map(|bucket| {
                    bucket
                        .providers
                        .get(&provider.provider)
                        .map_or(0.0, |value| view.measure(value.cost_usd, value.tokens))
                })
                .collect();
            let mut spans = vec![
                Span::styled("● ", style),
                Span::styled(
                    pad_right(&provider_label(&provider.provider), 12),
                    theme.text,
                ),
                Span::styled(
                    pad_left(&view.show(provider.cost_usd, provider.tokens), 8),
                    theme.text_strong,
                ),
                Span::styled(pad_left(&format_share(share), 6), theme.muted),
            ];
            if trend_width > 4 {
                spans.push(Span::raw("   "));
                spans.push(sparkline(&series, trend_width, style));
            }
            Line::from(spans)
        })
        .collect()
}
