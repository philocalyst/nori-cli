//! The header row: period tabs, the provider filter, the prices tab and the metric toggle.
//! Every chip is a click target.

use nori_harness::usage::UsagePeriod;
use nori_tui_components::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use super::format::provider_label;
use super::format::provider_style;
use super::view::Metric;
use super::view::ModelKey;

/// A clickable spot in the overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Hit {
    Period(UsagePeriod),
    Metric(Metric),
    /// Cycle the provider filter.
    Provider,
    Prices,
    Model(ModelKey),
}

pub(super) const PERIODS: [UsagePeriod; 3] =
    [UsagePeriod::Day, UsagePeriod::Week, UsagePeriod::Month];

pub(super) struct Header<'a> {
    pub theme: &'a Theme,
    pub period: UsagePeriod,
    pub metric: Metric,
    pub provider: Option<&'a str>,
    pub prices_open: bool,
}

struct Chip {
    spans: Vec<Span<'static>>,
    hit: Hit,
}

impl Chip {
    fn width(&self) -> u16 {
        self.spans
            .iter()
            .map(|span| u16::try_from(span.width()).unwrap_or(0))
            .sum()
    }
}

fn tab(theme: &Theme, label: &str, selected: bool, hit: Hit) -> Chip {
    let style = if selected {
        theme
            .accent_cyan
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        theme.muted
    };
    Chip {
        spans: vec![Span::styled(label.to_owned(), style)],
        hit,
    }
}

/// Lay `chips` out left to right from `x`, two cells apart, recording where each landed.
fn place(chips: Vec<Chip>, x: u16, y: u16, hits: &mut Vec<(Rect, Hit)>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut cursor = x;
    for (index, chip) in chips.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
            cursor += 2;
        }
        let width = chip.width();
        hits.push((Rect::new(cursor, y, width, 1), chip.hit));
        spans.extend(chip.spans);
        cursor += width;
    }
    spans
}

pub(super) fn render_header(
    header: &Header,
    row: Rect,
    buf: &mut Buffer,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let theme = header.theme;
    let mut left: Vec<Chip> = PERIODS
        .iter()
        .map(|period| {
            tab(
                theme,
                period.label(),
                *period == header.period,
                Hit::Period(*period),
            )
        })
        .collect();
    let provider = match header.provider {
        Some(provider) => Chip {
            spans: vec![
                Span::styled("● ", provider_style(theme, provider)),
                Span::styled(format!("{} ▾", provider_label(provider)), theme.text),
            ],
            hit: Hit::Provider,
        },
        None => Chip {
            spans: vec![Span::styled("all providers ▾", theme.muted)],
            hit: Hit::Provider,
        },
    };
    left.push(provider);
    let mut right = vec![tab(theme, "prices", header.prices_open, Hit::Prices)];
    right.extend([Metric::Cost, Metric::Tokens].map(|metric| {
        tab(
            theme,
            metric.label(),
            metric == header.metric,
            Hit::Metric(metric),
        )
    }));

    let right_width: u16 = right.iter().map(Chip::width).sum::<u16>() + 2 * 2;
    let right_x = row.right().saturating_sub(right_width);
    let left_spans = place(left, row.x, row.y, hits);
    let right_spans = place(right, right_x, row.y, hits);
    Paragraph::new(Line::from(left_spans)).render(
        Rect::new(row.x, row.y, right_x.saturating_sub(row.x), 1),
        buf,
    );
    Paragraph::new(Line::from(right_spans)).render(Rect::new(right_x, row.y, right_width, 1), buf);
}
