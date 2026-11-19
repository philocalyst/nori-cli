//! Vertical stacked bars drawn in terminal cells, eight levels per row.

use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

const EIGHTHS_UP: [&str; 9] = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];

/// One bar: segment values from the bottom up, each with its color.
pub type Bar = Vec<(f64, Color)>;

/// Render `bars` as `height` rows (top row first). Every bar is `bar_width` cells wide with `gap`
/// blank cells after it; heights are scaled so `max` fills the chart.
///
/// A cell where two segments meet is a partial block in the lower segment's color on the upper
/// one's background, so a stack of any number of segments stays exact to an eighth of a row.
pub fn stacked_bar_rows(
    bars: &[Bar],
    max: f64,
    height: i32,
    bar_width: i32,
    gap: i32,
) -> Vec<Line<'static>> {
    let height = height.max(1);
    let levels_total = f64::from(height) * 8.0;
    let max = if max > 0.0 { max } else { 1.0 };
    // Cumulative level boundaries per bar: (top level, color), bottom first.
    let stacks: Vec<Vec<(i32, Color)>> = bars
        .iter()
        .map(|bar| {
            let mut acc = 0.0;
            bar.iter()
                .filter(|(value, _)| *value > 0.0)
                .map(|(value, color)| {
                    acc += value;
                    ((acc / max * levels_total).round() as i32, *color)
                })
                .collect()
        })
        .collect();

    (0..height)
        .rev()
        .map(|row| {
            let (low, high) = (row * 8, row * 8 + 8);
            let mut spans: Vec<Span<'static>> = Vec::new();
            for stack in &stacks {
                // The segment containing `low` and the one above it within this cell.
                let mut below = None;
                let mut above = None;
                let mut start = 0;
                for (top, color) in stack {
                    let (from, to) = (start, *top);
                    start = *top;
                    if to <= low || from >= high {
                        continue;
                    }
                    if below.is_none() {
                        below = Some((to.min(high) - low, *color));
                    } else if above.is_none() {
                        above = Some(*color);
                    }
                }
                let cell = match (below, above) {
                    (None, _) => Span::raw(" ".repeat(usize::try_from(bar_width).unwrap_or(1))),
                    (Some((levels, color)), None) => {
                        let glyph = EIGHTHS_UP[usize::try_from(levels.clamp(0, 8)).unwrap_or(0)];
                        Span::styled(
                            glyph.repeat(usize::try_from(bar_width).unwrap_or(1)),
                            Style::new().fg(color),
                        )
                    }
                    (Some((levels, color)), Some(over)) => {
                        let glyph = EIGHTHS_UP[usize::try_from(levels.clamp(0, 8)).unwrap_or(0)];
                        Span::styled(
                            glyph.repeat(usize::try_from(bar_width).unwrap_or(1)),
                            Style::new().fg(color).bg(over),
                        )
                    }
                };
                spans.push(cell);
                if gap > 0 {
                    spans.push(Span::raw(" ".repeat(usize::try_from(gap).unwrap_or(0))));
                }
            }
            Line::from(spans)
        })
        .collect()
}
