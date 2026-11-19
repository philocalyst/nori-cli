//! A horizontal meter with eighth-cell resolution and an optional pace mark.

use ratatui::style::Style;
use ratatui::text::Span;

const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

/// Draw `percent` (0 to 100) across `width` cells: `████████░░░░░░░░`.
///
/// `pace`, when given, is how far through the window time has run (0 to 100). It is drawn as a
/// `┃` in the track, so used-ahead-of-pace reads at a glance.
pub fn meter_spans(
    percent: f64,
    width: i32,
    fill: Style,
    track: Style,
    pace: Option<(f64, Style)>,
) -> Vec<Span<'static>> {
    let width = width.max(1);
    let cells = f64::from(width) * percent.clamp(0.0, 100.0) / 100.0;
    let full = cells.floor() as i32;
    let partial = ((cells - f64::from(full)) * 8.0).round() as usize;
    let (full, partial) = if partial == 8 {
        (full + 1, 0)
    } else {
        (full, partial)
    };
    let pace_cell = pace.map(|(elapsed, style)| {
        (
            ((f64::from(width) * elapsed.clamp(0.0, 100.0) / 100.0).floor() as i32).min(width - 1),
            style,
        )
    });

    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style = fill;
    let flush = |run: &mut String, style: Style, spans: &mut Vec<Span<'static>>| {
        if !run.is_empty() {
            spans.push(Span::styled(std::mem::take(run), style));
        }
    };
    for cell in 0..width {
        let (glyph, style) = if let Some((at, style)) = pace_cell
            && at == cell
            && cell >= full + i32::from(partial > 0)
        {
            ("┃", style)
        } else if cell < full {
            ("█", fill)
        } else if cell == full && partial > 0 {
            (EIGHTHS[partial], fill)
        } else {
            ("░", track)
        };
        if style != run_style {
            flush(&mut run, run_style, &mut spans);
            run_style = style;
        }
        run.push_str(glyph);
    }
    flush(&mut run, run_style, &mut spans);
    spans
}
