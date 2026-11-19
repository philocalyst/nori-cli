//! One-line trend: `▁▂▄▇█▅▂`.

use ratatui::style::Style;
use ratatui::text::Span;

pub const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// `values` resampled to at most `width` cells (the maximum of each group), scaled to the largest
/// value. A zero value is a blank, so quiet periods read as gaps rather than floor-height bars.
pub fn sparkline(values: &[f64], width: i32, style: Style) -> Span<'static> {
    let width = usize::try_from(width.max(1)).unwrap_or(1);
    let cells: Vec<f64> = if values.len() <= width {
        values.to_vec()
    } else {
        (0..width)
            .map(|cell| {
                let from = cell * values.len() / width;
                let to = ((cell + 1) * values.len() / width).max(from + 1);
                values[from..to].iter().copied().fold(0.0, f64::max)
            })
            .collect()
    };
    let peak = cells.iter().copied().fold(0.0, f64::max);
    let text: String = cells
        .iter()
        .map(|value| {
            if peak <= 0.0 || *value <= 0.0 {
                ' '
            } else {
                let level = ((value / peak) * 7.0).round() as usize;
                LEVELS[level.min(7)]
            }
        })
        .collect();
    Span::styled(text, style)
}
