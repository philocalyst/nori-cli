//! One line of a diff as it is drawn: the line number in a faint gutter, a bar in the margin
//! that says how the line changed, and the styled content wrapped under itself within the width.
//! Nothing is padded or tinted past the text, so a terminal re-wrapping the conversation keeps
//! every row a line of text.

use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;
use unicode_width::UnicodeWidthChar;

use super::style::DiffRenderStyleContext;
use super::style::style_gutter;
use super::style::style_mark;

/// How a line of a diff changed, for the bar in its margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    Kept,
    Added,
    Removed,
    /// Edited in place: what it lost and gained are on it.
    Edited,
}

#[derive(Clone, Copy)]
pub(super) struct DiffLineLayout {
    pub(super) width: usize,
    pub(super) line_number_width: usize,
}

/// The bar in the margin of a changed line, drawn down every row it wraps to.
const BAR: &str = "▎";

/// One diff line as the rows it wraps to: `line_number` right-aligned in the gutter on the first
/// row (none for a line that is gone), the bar of its `mark`, and `content` wrapped under itself
/// within `layout.width`.
pub(super) fn diff_line_rows(
    line_number: Option<usize>,
    mark: Mark,
    content: Vec<RtSpan<'static>>,
    layout: DiffLineLayout,
    ctx: &DiffRenderStyleContext,
) -> Vec<RtLine<'static>> {
    let gutter_width = layout.line_number_width.max(1);
    // The number and a space, the bar, a space.
    let available = layout.width.saturating_sub(gutter_width + 3).max(1);
    let bar = match mark {
        Mark::Kept => RtSpan::raw(" "),
        Mark::Added | Mark::Removed | Mark::Edited => RtSpan::styled(BAR, style_mark(ctx, mark)),
    };
    wrap_styled_spans(&content, available)
        .into_iter()
        .enumerate()
        .map(|(row, chunk)| {
            let number = match line_number {
                Some(number) if row == 0 => format!("{number:>gutter_width$} "),
                Some(_) | None => format!("{:gutter_width$} ", ""),
            };
            let mut spans = vec![
                RtSpan::styled(number, style_gutter(ctx)),
                bar.clone(),
                RtSpan::raw(" "),
            ];
            spans.extend(chunk);
            RtLine::from(spans)
        })
        .collect()
}

fn wrap_styled_spans(spans: &[RtSpan<'static>], max_cols: usize) -> Vec<Vec<RtSpan<'static>>> {
    let max_cols = max_cols.max(1);
    let mut rows: Vec<Vec<RtSpan<'static>>> = Vec::new();
    let mut current: Vec<RtSpan<'static>> = Vec::new();
    let mut current_cols = 0;

    for span in spans {
        let style = span.style;
        let mut buf = String::new();
        let mut buf_cols = 0;

        for ch in span.content.chars() {
            let ch_cols = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_cols + buf_cols + ch_cols > max_cols
                && (!current.is_empty() || !buf.is_empty())
            {
                if !buf.is_empty() {
                    current.push(RtSpan::styled(std::mem::take(&mut buf), style));
                    buf_cols = 0;
                }
                rows.push(std::mem::take(&mut current));
                current_cols = 0;
            }

            buf.push(ch);
            buf_cols += ch_cols;

            if current_cols + buf_cols == max_cols {
                current.push(RtSpan::styled(std::mem::take(&mut buf), style));
                rows.push(std::mem::take(&mut current));
                current_cols = 0;
                buf_cols = 0;
            }
        }

        if !buf.is_empty() {
            current_cols += buf_cols;
            current.push(RtSpan::styled(buf, style));
        }
    }

    if !current.is_empty() {
        rows.push(current);
    }
    if rows.is_empty() {
        rows.push(vec![RtSpan::from(String::new())]);
    }
    rows
}

pub(super) fn line_number_width(max_line_number: usize) -> usize {
    if max_line_number == 0 {
        1
    } else {
        max_line_number.to_string().len()
    }
}
