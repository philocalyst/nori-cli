//! One model in detail: token types, cost by type, its own chart, the price in effect and how
//! to override it.

use nori_harness::usage::ModelUsage;
use nori_harness::usage::PriceSource;
use nori_tui_components::charts::Bar;
use nori_tui_components::charts::ShareSegment;
use nori_tui_components::charts::share_bar_spans;
use nori_tui_components::charts::share_legend_lines;
use ratatui::text::Line;
use ratatui::text::Span;

use super::chart::plot;
use super::format::color_of;
use super::format::format_share;
use super::format::format_tokens;
use super::format::format_usd;
use super::format::pad_left;
use super::format::pad_right;
use super::format::provider_label;
use super::format::provider_style;
use super::view::Metric;
use super::view::ModelKey;
use super::view::View;

const CHART_HEIGHT: i32 = 4;

pub(super) fn detail(view: &View, key: &ModelKey) -> Vec<Line<'static>> {
    let theme = view.theme;
    let Some(model) = view
        .summary
        .models
        .iter()
        .find(|model| model.provider == key.0 && model.model == key.1)
    else {
        return vec![
            header(view, &key.0, &key.1, false),
            Line::default(),
            Line::from(Span::styled(
                format!(
                    "No usage for {} in the last {}.",
                    key.1,
                    view.summary.period.label()
                ),
                theme.text_secondary,
            )),
        ];
    };
    let custom = model.price_source == Some(PriceSource::Override);
    let mut lines = vec![header(view, &model.provider, &model.model, custom)];
    lines.extend(headline(view, model));
    for section in [
        token_types(view, model),
        series(view, model),
        price(view, model),
    ] {
        lines.push(Line::default());
        lines.extend(section);
    }
    lines
}

fn header(view: &View, provider: &str, model: &str, custom: bool) -> Line<'static> {
    let theme = view.theme;
    let mut spans = vec![
        Span::styled("‹ ", theme.faint),
        Span::styled(model.to_owned(), theme.text_strong),
        Span::styled("  ● ", provider_style(theme, provider)),
        Span::styled(provider_label(provider), theme.text_secondary),
    ];
    if custom {
        spans.push(Span::styled("  custom price", theme.accent_amber));
    }
    Line::from(spans)
}

fn headline(view: &View, model: &ModelUsage) -> Vec<Line<'static>> {
    let theme = view.theme;
    let measured = view.measure(model.cost_usd, model.tokens.total());
    let total: f64 = view
        .summary
        .models
        .iter()
        .map(|other| view.measure(other.cost_usd, other.tokens.total()))
        .sum();
    let share = if total > 0.0 { measured / total } else { 0.0 };
    let other = match view.metric {
        Metric::Cost => format!("{} tokens", format_tokens(model.tokens.total())),
        Metric::Tokens => format!("{} at API prices", format_usd(model.cost_usd)),
    };
    vec![
        Line::from(Span::styled(
            view.show(model.cost_usd, model.tokens.total()),
            theme
                .text_strong
                .add_modifier(ratatui::style::Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(other, theme.muted),
            Span::styled(" · ", theme.faint),
            Span::styled(
                format!(
                    "{} session{}",
                    model.sessions,
                    if model.sessions == 1 { "" } else { "s" }
                ),
                theme.muted,
            ),
            Span::styled(" · ", theme.faint),
            Span::styled(
                format!("{} of {}", format_share(share), view.metric.label()),
                theme.muted,
            ),
        ]),
    ]
}

fn token_types(view: &View, model: &ModelUsage) -> Vec<Line<'static>> {
    let theme = view.theme;
    let tokens = &model.tokens;
    let cost = &model.category_cost;
    let rows = [
        ("input", tokens.input, cost.input, theme.accent_cyan),
        (
            "cache read",
            tokens.cache_read,
            cost.cache_read,
            theme.accent_mint,
        ),
        (
            "cache write",
            tokens.cache_write,
            cost.cache_write,
            theme.accent_violet,
        ),
        ("output", tokens.output, cost.output, theme.accent_amber),
    ];
    let total = tokens.total().max(1) as f64;
    let priced = model.price.is_some();
    let mut lines = vec![
        view.section_title("By token type"),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(pad_right("type", 14), theme.faint),
            Span::styled(pad_left("tokens", 9), theme.faint),
            Span::styled(pad_left("share", 8), theme.faint),
            Span::styled(pad_left("cost", 10), theme.faint),
        ]),
    ];
    let mut segments = Vec::new();
    for (label, count, usd, style) in rows {
        lines.push(Line::from(vec![
            Span::styled("■ ", style),
            Span::styled(pad_right(label, 14), theme.text_secondary),
            Span::styled(pad_left(&format_tokens(count), 9), theme.text),
            Span::styled(
                pad_left(&format_share(count as f64 / total), 8),
                theme.muted,
            ),
            Span::styled(
                pad_left(
                    &if priced {
                        format_usd(usd)
                    } else {
                        "–".to_owned()
                    },
                    10,
                ),
                theme.text_strong,
            ),
        ]));
        segments.push(ShareSegment {
            label: label.to_owned(),
            value: match view.metric {
                Metric::Cost => usd,
                Metric::Tokens => count as f64,
            },
            shown: match view.metric {
                Metric::Cost => format_usd(usd),
                Metric::Tokens => format_tokens(count),
            },
            style,
        });
    }
    if tokens.reasoning > 0 {
        lines.push(Line::from(Span::styled(
            format!(
                "  of which reasoning {} (already in output)",
                format_tokens(tokens.reasoning)
            ),
            theme.faint,
        )));
    }
    if segments.iter().any(|segment| segment.value > 0.0) {
        lines.push(Line::default());
        lines.push(Line::from(share_bar_spans(&segments, view.width)));
        lines.extend(share_legend_lines(
            &segments,
            view.width,
            theme.muted,
            theme.text,
        ));
    }
    lines
}

fn series(view: &View, model: &ModelUsage) -> Vec<Line<'static>> {
    let theme = view.theme;
    let color = color_of(provider_style(theme, &model.provider));
    let bars: Vec<Bar> = model
        .series
        .iter()
        .map(|value| vec![(view.measure(value.cost_usd, value.tokens), color)])
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
    lines
}

fn price(view: &View, model: &ModelUsage) -> Vec<Line<'static>> {
    let theme = view.theme;
    let mut lines = vec![view.section_title("Price")];
    let key = format!("{}:{}", model.provider, model.model);
    let Some(price) = model.price else {
        lines.push(Line::from(Span::styled(
            "No price is known for this model: its tokens count, its cost does not.",
            theme.text_secondary,
        )));
        lines.extend(snippet(view, &key, None));
        return lines;
    };
    let source = match model.price_source {
        Some(PriceSource::Override) => Span::styled("custom", theme.accent_amber),
        Some(PriceSource::Builtin) | None => Span::styled("list price", theme.muted),
    };
    lines.push(Line::from(vec![
        Span::styled("USD per million tokens · ", theme.faint),
        source,
    ]));
    let cell = |label: &str, value: f64| {
        vec![
            Span::styled(format!("{label} "), theme.faint),
            Span::styled(pad_right(&format_usd(value), 9), theme.text),
        ]
    };
    let mut row = Vec::new();
    for (label, value) in [
        ("input", price.input),
        ("output", price.output),
        ("cache read", price.cache_read),
        ("cache write", price.cache_write),
    ] {
        row.extend(cell(label, value));
    }
    lines.push(Line::from(row));
    lines.push(Line::from(vec![
        Span::styled("Cache savings ", theme.faint),
        Span::styled(format_usd(model.cache_savings_usd), theme.accent_mint),
        Span::styled(
            format!(
                "   {} of the cost is estimated at this price",
                format_usd(model.estimated_cost_usd)
            ),
            theme.faint,
        ),
    ]));
    if model.price_source != Some(PriceSource::Override) {
        lines.extend(snippet(view, &key, Some(price)));
    } else {
        lines.push(Line::from(Span::styled(
            format!("Set by [usage.prices.\"{key}\"] in config.toml; remove the table to use the list price."),
            theme.faint,
        )));
    }
    lines
}

/// How to override the price: the table to add to `config.toml`. Editing happens in the file.
fn snippet(
    view: &View,
    key: &str,
    price: Option<nori_harness::usage::ModelPrice>,
) -> Vec<Line<'static>> {
    let theme = view.theme;
    let (input, output) = price.map_or((0.0, 0.0), |price| (price.input, price.output));
    vec![
        Line::from(Span::styled(
            "To set your own, add to $NORI_HOME/config.toml:",
            theme.faint,
        )),
        Line::from(Span::styled(
            format!("  [usage.prices.\"{key}\"]"),
            theme.code,
        )),
        Line::from(Span::styled(
            format!("  input = {input:.2}  output = {output:.2}"),
            theme.code,
        )),
    ]
}
