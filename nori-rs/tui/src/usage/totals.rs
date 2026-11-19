//! The totals row: processed, cached input, uncached input, output and cache savings.

use ratatui::text::Line;
use ratatui::text::Span;

use super::format::format_tokens;
use super::format::format_usd;
use super::format::pad_right;
use super::view::View;

pub(super) fn totals(view: &View) -> Vec<Line<'static>> {
    let totals = &view.summary.totals;
    let tokens = &totals.tokens;
    let width = (view.width / 5).max(12);
    // Long labels when five of them fit side by side, short ones otherwise.
    let pick = |long: &'static str, short: &'static str| if width >= 16 { long } else { short };
    let cells = [
        (
            pick("Processed", "Processed"),
            format_tokens(tokens.total()),
        ),
        (
            pick("Cached input", "Cached"),
            format_tokens(tokens.cache_read),
        ),
        (
            pick("Uncached input", "Uncached"),
            format_tokens(tokens.input.saturating_add(tokens.cache_write)),
        ),
        (pick("Output", "Output"), format_tokens(tokens.output)),
        (
            pick("Cache savings", "Savings"),
            format_usd(totals.cache_savings_usd),
        ),
    ];
    let mut labels = Vec::new();
    let mut values = Vec::new();
    for (label, value) in cells {
        labels.push(Span::styled(pad_right(label, width), view.theme.faint));
        values.push(Span::styled(pad_right(&value, width), view.theme.text));
    }
    vec![Line::from(labels), Line::from(values)]
}
