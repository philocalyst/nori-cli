//! The effective price table: what each model seen is priced by, and the overrides in
//! `config.toml` that nothing has used yet. Read-only: the source of truth is the config file.

use nori_harness::usage::PriceSource;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::clip;
use super::format::format_usd;
use super::format::pad_left;
use super::format::pad_right;
use super::format::provider_style;
use super::view::View;

pub(super) fn prices(view: &View) -> Vec<Line<'static>> {
    let theme = view.theme;
    let name_width = (view.width - 2 - 41 - 2 - 8).clamp(14, 40);
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Prices", theme.text_strong),
            Span::styled("  USD per million tokens", theme.muted),
        ]),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(pad_right("model", name_width), theme.faint),
            Span::styled(pad_left("input", 9), theme.faint),
            Span::styled(pad_left("output", 9), theme.faint),
            Span::styled(pad_left("cache read", 11), theme.faint),
            Span::styled(pad_left("cache write", 12), theme.faint),
            Span::styled("  source", theme.faint),
        ]),
    ];
    if view.summary.prices.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No priced models in this period.",
            theme.faint,
        )));
    }
    for row in &view.summary.prices {
        let (mark, name_style) = match row.provider.as_deref() {
            Some(provider) if row.seen => ("● ", provider_style(theme, provider)),
            Some(_) | None => ("○ ", theme.faint),
        };
        let label = match &row.provider {
            Some(provider) if !row.seen => format!("{provider}:{}", row.model),
            Some(_) | None => row.model.clone(),
        };
        let custom = row.source == PriceSource::Override;
        let mut spans = vec![
            Span::styled(mark, name_style),
            Span::styled(pad_right(&clip(&label, name_width), name_width), theme.text),
        ];
        for (value, width) in [
            (row.price.input, 9),
            (row.price.output, 9),
            (row.price.cache_read, 11),
            (row.price.cache_write, 12),
        ] {
            spans.push(Span::styled(
                pad_left(&format_usd(value), width),
                if custom {
                    theme.text_strong
                } else {
                    theme.text
                },
            ));
        }
        spans.push(if custom {
            Span::styled(
                if row.seen { "  custom" } else { "  unused" },
                theme.accent_amber,
            )
        } else {
            Span::styled("  list", theme.faint)
        });
        lines.push(Line::from(spans));
    }
    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        "Read-only here. Override a model in $NORI_HOME/config.toml:",
        theme.faint,
    )));
    for text in [
        "  [usage.prices.\"<provider>:<model>\"]",
        "  input = 3.0  output = 15.0  cache_read = 0.3  cache_write = 3.75",
    ] {
        lines.push(Line::from(Span::styled(text, theme.code)));
    }
    lines
}
