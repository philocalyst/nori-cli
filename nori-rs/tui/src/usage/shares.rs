//! Share bars: where the cost or the tokens went, by model and by token type.

use nori_tui_components::charts::ShareSegment;
use nori_tui_components::charts::share_bar_spans;
use nori_tui_components::charts::share_legend_lines;
use ratatui::text::Line;

use super::format::format_tokens;
use super::format::format_usd;
use super::format::series_style;
use super::view::Metric;
use super::view::View;

const MODELS_SHOWN: usize = 5;

pub(super) fn shares(view: &View) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let by_model = by_model(view);
    let by_type = by_type(view);
    for (title, segments) in [("By model", by_model), ("By token type", by_type)] {
        if segments.iter().all(|segment| segment.value <= 0.0) {
            continue;
        }
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(view.section_title(title));
        lines.push(Line::from(share_bar_spans(&segments, view.width)));
        lines.extend(share_legend_lines(
            &segments,
            view.width,
            view.theme.muted,
            view.theme.text,
        ));
    }
    lines
}

fn by_model(view: &View) -> Vec<ShareSegment> {
    let mut models: Vec<_> = view.summary.models.iter().collect();
    models.sort_by(|a, b| {
        view.measure(b.cost_usd, b.tokens.total())
            .total_cmp(&view.measure(a.cost_usd, a.tokens.total()))
    });
    let mut segments: Vec<ShareSegment> = models
        .iter()
        .take(MODELS_SHOWN)
        .enumerate()
        .map(|(index, model)| ShareSegment {
            label: model.model.clone(),
            value: view.measure(model.cost_usd, model.tokens.total()),
            shown: view.show(model.cost_usd, model.tokens.total()),
            style: series_style(view.theme, index),
        })
        .collect();
    let rest: Vec<_> = models.iter().skip(MODELS_SHOWN).collect();
    if !rest.is_empty() {
        let cost: f64 = rest.iter().map(|model| model.cost_usd).sum();
        let tokens: i64 = rest.iter().map(|model| model.tokens.total()).sum();
        segments.push(ShareSegment {
            label: format!("{} others", rest.len()),
            value: view.measure(cost, tokens),
            shown: view.show(cost, tokens),
            style: view.theme.faint,
        });
    }
    segments
}

fn by_type(view: &View) -> Vec<ShareSegment> {
    let theme = view.theme;
    let tokens = &view.summary.totals.tokens;
    let cost = &view.summary.category_cost;
    let entries = [
        ("input", cost.input, tokens.input, theme.accent_cyan),
        (
            "cache read",
            cost.cache_read,
            tokens.cache_read,
            theme.accent_mint,
        ),
        (
            "cache write",
            cost.cache_write,
            tokens.cache_write,
            theme.accent_violet,
        ),
        ("output", cost.output, tokens.output, theme.accent_amber),
    ];
    entries
        .into_iter()
        .map(|(label, cost, tokens, style)| ShareSegment {
            label: label.to_owned(),
            value: match view.metric {
                Metric::Cost => cost,
                Metric::Tokens => tokens as f64,
            },
            shown: match view.metric {
                Metric::Cost => format_usd(cost),
                Metric::Tokens => format_tokens(tokens),
            },
            style,
        })
        .collect()
}
