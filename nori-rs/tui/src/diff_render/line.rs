//! One line of a diff as it is drawn: its content styled (syntax colours, the line's tint, the
//! changed words emphasised), then the right-aligned line number and sign in front, wrapped to
//! the width.

use core::range::Range;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::style::DiffRenderStyleContext;
use super::style::style_add;
use super::style::style_context;
use super::style::style_del;
use super::style::style_gutter;
use super::style::style_sign;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffLineType {
    Insert,
    Delete,
    Context,
}

#[derive(Clone, Copy)]
pub(super) struct DiffLineLayout {
    pub(super) width: usize,
    pub(super) line_number_width: usize,
}

/// The content of one diff line styled for drawing: the highlighter's colours where `syntax`
/// has them (dimmed on a removed line) and the line's own colour elsewhere, on the line's tint,
/// except that the `emphasis` byte ranges of `text` (sorted, disjoint) are drawn with the
/// stronger emphasis instead, keeping their syntax colours.
pub(super) fn content_spans(
    kind: DiffLineType,
    text: &str,
    syntax: Option<&[RtSpan<'static>]>,
    emphasis: &[Range<usize>],
    ctx: &DiffRenderStyleContext,
) -> Vec<RtSpan<'static>> {
    let (plain, tint, emphasized) = match kind {
        DiffLineType::Insert => (style_add(ctx), ctx.add_bg, ctx.add_emphasis),
        DiffLineType::Delete => (style_del(ctx), ctx.del_bg, ctx.del_emphasis),
        DiffLineType::Context => (style_context(), None, Style::default()),
    };
    // The highlighter's spans cover the line in order: as classes, they are its byte ranges.
    let mut at = 0;
    let classes: Vec<(Range<usize>, Style)> = syntax
        .unwrap_or_default()
        .iter()
        .map(|span| {
            let start = at;
            at += span.content.len();
            (Range::from(start..at), span.style)
        })
        .collect();
    disparate::layout::segments(Range::from(0..text.len()), &classes, emphasis)
        .map(|segment| {
            let style = match segment.class {
                // A removed line's syntax colours are dimmed, but for the words that changed.
                Some(class) if kind == DiffLineType::Delete && !segment.changed => {
                    class.add_modifier(Modifier::DIM)
                }
                Some(class) => class,
                None => plain,
            };
            let style = if segment.changed {
                style.patch(emphasized)
            } else {
                tint.map_or(style, |bg| style.bg(bg))
            };
            let bytes = &text.as_bytes()[segment.range];
            RtSpan::styled(String::from_utf8_lossy(bytes).into_owned(), style)
        })
        .collect()
}

/// One diff line as the rows it wraps to: the line number right-aligned in the gutter and the
/// sign on the first row, the styled `content` wrapped under the sign within `layout.width`,
/// and on a tinted line the tint carried through the gutter to the right edge.
pub(super) fn diff_line_rows(
    line_number: usize,
    kind: DiffLineType,
    content: Vec<RtSpan<'static>>,
    layout: DiffLineLayout,
    ctx: &DiffRenderStyleContext,
) -> Vec<RtLine<'static>> {
    let ln_str = line_number.to_string();

    // Reserve a fixed number of spaces (equal to the widest line number plus a
    // trailing spacer) so the sign column stays aligned across the diff block.
    let gutter_width = layout.line_number_width.max(1);
    let prefix_cols = gutter_width + 1;

    let mut first = true;
    let (sign_char, sign_style) = match kind {
        DiffLineType::Insert => ('+', style_sign(ctx, true)),
        DiffLineType::Delete => ('-', style_sign(ctx, false)),
        DiffLineType::Context => (' ', style_context()),
    };

    // Build a line-level background style so the bg extends edge-to-edge.
    let line_bg_style = match kind {
        DiffLineType::Insert => ctx.add_bg.map(|bg| Style::default().bg(bg)),
        DiffLineType::Delete => ctx.del_bg.map(|bg| Style::default().bg(bg)),
        DiffLineType::Context => None,
    };

    let mut lines: Vec<RtLine<'static>> = Vec::new();
    let available_content_cols = layout.width.saturating_sub(prefix_cols + 1).max(1);
    let wrapped_chunks = wrap_styled_spans(&content, available_content_cols);

    for chunk in wrapped_chunks {
        let (gutter_span, sign_span, used_cols) = if first {
            let gutter = format!("{ln_str:>gutter_width$} ");
            let sign = sign_char.to_string();
            let cols = UnicodeWidthStr::width(gutter.as_str())
                + UnicodeWidthStr::width(sign.as_str())
                + spans_display_width(&chunk);
            first = false;
            (gutter, Some(sign), cols)
        } else {
            let gutter = format!("{:gutter_width$}  ", "");
            let cols = UnicodeWidthStr::width(gutter.as_str()) + spans_display_width(&chunk);
            (gutter, None, cols)
        };

        // When a background tint is active, apply it to the gutter and sign (the content
        // carries its own) and pad with trailing spaces so the color fills the complete
        // width; the line's style carries it through any indent put in front of it.
        let line = if let Some(bg_style) = line_bg_style {
            let gutter_style = style_gutter(ctx).patch(bg_style);
            let mut spans = vec![RtSpan::styled(gutter_span, gutter_style)];
            if let Some(sign) = sign_span {
                spans.push(RtSpan::styled(sign, sign_style));
            }
            spans.extend(chunk);
            let pad = layout.width.saturating_sub(used_cols);
            if pad > 0 {
                spans.push(RtSpan::styled(" ".repeat(pad), bg_style));
            }
            RtLine::from(spans).style(bg_style)
        } else {
            let mut spans = vec![RtSpan::styled(gutter_span, style_gutter(ctx))];
            if let Some(sign) = sign_span {
                spans.push(RtSpan::styled(sign, sign_style));
            }
            spans.extend(chunk);
            RtLine::from(spans)
        };

        lines.push(line);
    }
    lines
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

fn spans_display_width(spans: &[RtSpan<'static>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

pub(super) fn line_number_width(max_line_number: usize) -> usize {
    if max_line_number == 0 {
        1
    } else {
        max_line_number.to_string().len()
    }
}
