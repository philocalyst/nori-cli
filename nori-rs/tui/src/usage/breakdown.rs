//! The per-model table: rank, model, cost, share, tokens, and a thin bar. A row is selectable;
//! Enter opens the model's detail.

use nori_harness::usage::PriceSource;
use ratatui::text::Line;
use ratatui::text::Span;

use super::format::clip;
use super::format::format_share;
use super::format::format_tokens;
use super::format::format_usd;
use super::format::pad_left;
use super::format::pad_right;
use super::format::provider_style;
use super::view::ModelKey;
use super::view::View;
use super::view::ranked_models;

const NUMBER_COLUMNS: i32 = 9 + 7 + 8;
const CUSTOM_PRICE: &str = " custom price";

/// The table, and where each model's row is: `(line within the table, model)`.
pub(super) struct Breakdown {
    pub lines: Vec<Line<'static>>,
    pub rows: Vec<(usize, ModelKey)>,
}

pub(super) fn breakdown(view: &View) -> Breakdown {
    let theme = view.theme;
    let models = ranked_models(view.summary, view.metric);
    let total: f64 = models
        .iter()
        .map(|model| view.measure(model.cost_usd, model.tokens.total()))
        .sum();
    let peak = models.first().map_or(0.0, |model| {
        view.measure(model.cost_usd, model.tokens.total())
    });
    let bar_width = (view.width - 4 - NUMBER_COLUMNS - 42).clamp(0, 24);
    let name_width = (view.width - 4 - NUMBER_COLUMNS - bar_width - 2).clamp(12, 40);

    let mut lines = vec![
        view.section_title("Models"),
        Line::from(vec![
            Span::styled(pad_left("#", 3), theme.faint),
            Span::raw("   "),
            Span::styled(pad_right("model", name_width - 2), theme.faint),
            Span::styled(pad_left("cost", 9), theme.faint),
            Span::styled(pad_left("share", 7), theme.faint),
            Span::styled(pad_left("tokens", 8), theme.faint),
        ]),
    ];
    let mut rows = Vec::new();
    for (index, model) in models.into_iter().enumerate() {
        let key: ModelKey = (model.provider.clone(), model.model.clone());
        let selected = view.selected == Some(&key);
        let measured = view.measure(model.cost_usd, model.tokens.total());
        let style = provider_style(theme, &model.provider);
        let custom = model.price_source == Some(PriceSource::Override)
            && name_width - 2
                > i32::try_from(model.model.chars().count() + CUSTOM_PRICE.len()).unwrap_or(0);
        let name = clip(&model.model, name_width - 2);
        let name_cells = i32::try_from(name.chars().count()).unwrap_or(0);
        let cost = if model.cost_usd <= 0.0 && model.unpriced_tokens > 0 {
            Span::styled(pad_left("unpriced", 9), theme.faint)
        } else {
            Span::styled(pad_left(&format_usd(model.cost_usd), 9), theme.text_strong)
        };
        let rank = format!("{}", index + 1);
        let mut spans = vec![
            if selected {
                Span::styled("▌", theme.pointer)
            } else {
                Span::raw(" ")
            },
            Span::styled(pad_left(&rank, 2), theme.faint),
            Span::raw(" "),
            Span::styled("● ", style),
            Span::styled(
                name,
                if selected {
                    theme.text_strong
                } else {
                    theme.text
                },
            ),
        ];
        if custom {
            spans.push(Span::styled(CUSTOM_PRICE, theme.faint));
        }
        let used = name_cells
            + if custom {
                i32::try_from(CUSTOM_PRICE.len()).unwrap_or(0)
            } else {
                0
            };
        spans.push(Span::raw(" ".repeat(
            usize::try_from((name_width - 2 - used).max(0)).unwrap_or(0),
        )));
        spans.push(cost);
        spans.push(Span::styled(
            pad_left(
                &format_share(if total > 0.0 { measured / total } else { 0.0 }),
                7,
            ),
            theme.muted,
        ));
        spans.push(Span::styled(
            pad_left(&format_tokens(model.tokens.total()), 8),
            theme.muted,
        ));
        if bar_width > 0 && peak > 0.0 {
            let cells = ((measured / peak) * f64::from(bar_width)).ceil().max(1.0) as usize;
            spans.push(Span::raw("  "));
            spans.push(Span::styled("━".repeat(cells), style));
        }
        rows.push((lines.len(), key));
        let line = Line::from(spans);
        lines.push(if selected {
            line.style(theme.selected)
        } else {
            line
        });
    }
    Breakdown { lines, rows }
}
