//! A proportional bar and its legend: `━━━━━━━━━━━━━━━━━━━━━━` over `■ input 42%  ■ output 12%`.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

/// One slice of a share bar.
#[derive(Debug, Clone)]
pub struct ShareSegment {
    pub label: String,
    pub value: f64,
    /// The value as the legend prints it ("1.2M", "$4.20").
    pub shown: String,
    pub style: Style,
}

/// Cell widths for `segments` summing to `width`: proportional by largest remainder, every
/// non-zero segment at least one cell.
fn cell_widths(segments: &[&ShareSegment], width: i32) -> Vec<i32> {
    let total: f64 = segments.iter().map(|segment| segment.value).sum();
    if total <= 0.0 || segments.is_empty() {
        return Vec::new();
    }
    let exact: Vec<f64> = segments
        .iter()
        .map(|segment| segment.value / total * f64::from(width))
        .collect();
    let mut cells: Vec<i32> = exact
        .iter()
        .map(|value| (value.floor() as i32).max(1))
        .collect();
    while cells.iter().sum::<i32>() < width {
        let Some(index) = exact
            .iter()
            .enumerate()
            .max_by(|(a, x), (b, y)| {
                (*x - f64::from(cells[*a])).total_cmp(&(*y - f64::from(cells[*b])))
            })
            .map(|(index, _)| index)
        else {
            break;
        };
        cells[index] += 1;
    }
    while cells.iter().sum::<i32>() > width {
        let Some(index) = cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| **cell > 1)
            .max_by_key(|(_, cell)| **cell)
            .map(|(index, _)| index)
        else {
            break;
        };
        cells[index] -= 1;
    }
    cells
}

/// The bar, one cell-run per non-zero segment.
pub fn share_bar_spans(segments: &[ShareSegment], width: i32) -> Vec<Span<'static>> {
    let visible: Vec<&ShareSegment> = segments
        .iter()
        .filter(|segment| segment.value > 0.0)
        .collect();
    cell_widths(&visible, width.max(1))
        .into_iter()
        .zip(visible)
        .map(|(cells, segment)| {
            Span::styled(
                "━".repeat(usize::try_from(cells).unwrap_or(1)),
                segment.style,
            )
        })
        .collect()
}

/// The legend, wrapped to `width`: `■ label value` entries, two spaces apart.
pub fn share_legend_lines(
    segments: &[ShareSegment],
    width: i32,
    label_style: Style,
    value_style: Style,
) -> Vec<Line<'static>> {
    let width = usize::try_from(width.max(1)).unwrap_or(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for segment in segments.iter().filter(|segment| segment.value > 0.0) {
        let entry_width = 2 + segment.label.width() + 1 + segment.shown.width();
        let separator = if current.is_empty() { 0 } else { 2 };
        if used > 0 && used + separator + entry_width > width {
            lines.push(Line::from(std::mem::take(&mut current)));
            used = 0;
        }
        if !current.is_empty() {
            current.push(Span::raw("  "));
            used += 2;
        }
        current.push(Span::styled("■ ", segment.style));
        current.push(Span::styled(segment.label.clone(), label_style));
        current.push(Span::raw(" "));
        current.push(Span::styled(segment.shown.clone(), value_style));
        used += entry_width;
    }
    if !current.is_empty() {
        lines.push(Line::from(current));
    }
    lines
}
