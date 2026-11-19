//! Side-by-side split diff rendering for wide terminals (`>= 120` columns).

use core::range::Range;
use std::path::Path;

use disparate::Both;
use disparate::Side;
use ratatui::style::Style;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::highlighted_diff_lines_for_path;
use super::line::DiffLineType;
use super::line::content_spans;
use super::line::line_number_width;
use super::style::DiffRenderStyleContext;
use super::style::style_context;
use super::style::style_gutter;
use super::style::style_sign;
use super::update::EditDiff;
use super::update::Shown;

/// Minimum outer terminal/render width that activates side-by-side split diffs: each half still
/// has room for a readable line.
pub(super) const SPLIT_DIFF_MIN_WIDTH: usize = 120;

/// The divider between the panes: one faint rule with a column of air either side.
const DIVIDER: &str = " │ ";
/// Columns of [`DIVIDER`].
const DIVIDER_WIDTH: usize = 3;

struct SplitCell<'a> {
    line_num: Option<usize>,
    kind: DiffLineType,
    spans: Vec<RtSpan<'static>>,
    _marker: std::marker::PhantomData<&'a ()>,
}

pub(super) fn render_split(
    diff: &EditDiff<'_>,
    groups: &[(Both<Range<usize>>, Vec<Shown>)],
    first_line: usize,
    path: &Path,
    width: usize,
    ctx: &DiffRenderStyleContext,
    out: &mut Vec<RtLine<'static>>,
) {
    // Two panes of one width, a single faint divider between them; the same gutter, padding and
    // sign on both sides.
    let half = width.saturating_sub(DIVIDER_WIDTH) / 2;
    let max_line_number = groups
        .iter()
        .flat_map(|(_, shown)| shown)
        .map(|s| s.line.max(s.before_line) + first_line)
        .max()
        .unwrap_or(0);
    let gutter_w = line_number_width(max_line_number).max(1);

    for (index, (window, shown)) in groups.iter().enumerate() {
        if index > 0 {
            out.push(RtLine::from(vec![
                RtSpan::raw(" ".repeat(half)),
                RtSpan::styled(" ⋮ ", style_gutter(ctx)),
            ]));
        }

        let syntax = Both::new(Side::Before, Side::After).map(|side| {
            let span = diff.texts()[side].span(window[side]);
            let slice = &diff.sources()[side][span.start..span.end];
            (!slice.is_empty())
                .then(|| highlighted_diff_lines_for_path(slice, path))
                .flatten()
        });

        let pairs = pair_hunk_rows(shown);
        for (left_item, right_item) in pairs {
            let l_cell = build_cell(
                diff,
                window,
                &syntax,
                left_item,
                Side::Before,
                first_line,
                ctx,
            );
            let r_cell = build_cell(
                diff,
                window,
                &syntax,
                right_item,
                Side::After,
                first_line,
                ctx,
            );
            let mut spans = format_half(l_cell, half, gutter_w, ctx);
            spans.push(RtSpan::styled(DIVIDER, style_gutter(ctx)));
            spans.extend(format_half(r_cell, half, gutter_w, ctx));
            out.push(RtLine::from(spans));
        }
    }
}

fn pair_hunk_rows(shown: &[Shown]) -> Vec<(Option<&Shown>, Option<&Shown>)> {
    let mut pairs = Vec::new();
    let mut i = 0;
    while i < shown.len() {
        if shown[i].kind == DiffLineType::Context {
            pairs.push((Some(&shown[i]), Some(&shown[i])));
            i += 1;
            continue;
        }
        let del_start = i;
        while i < shown.len() && shown[i].kind == DiffLineType::Delete {
            i += 1;
        }
        let del_end = i;
        let ins_start = i;
        while i < shown.len() && shown[i].kind == DiffLineType::Insert {
            i += 1;
        }
        let ins_end = i;
        let del_len = del_end - del_start;
        let ins_len = ins_end - ins_start;
        let count = del_len.max(ins_len);
        for offset in 0..count {
            let left = (offset < del_len).then(|| &shown[del_start + offset]);
            let right = (offset < ins_len).then(|| &shown[ins_start + offset]);
            pairs.push((left, right));
        }
    }
    pairs
}

fn build_cell<'a>(
    diff: &'a EditDiff<'a>,
    window: &Both<Range<usize>>,
    syntax: &Both<Option<Vec<Vec<RtSpan<'static>>>>>,
    item: Option<&Shown>,
    side: Side,
    first_line: usize,
    ctx: &DiffRenderStyleContext,
) -> Option<SplitCell<'a>> {
    let item = item?;
    let line_idx = match (item.kind, side) {
        (DiffLineType::Context, Side::Before) => item.before_line,
        (DiffLineType::Context, Side::After) => item.line,
        (DiffLineType::Delete, Side::Before) => item.line,
        (DiffLineType::Insert, Side::After) => item.line,
        (DiffLineType::Delete, Side::After) | (DiffLineType::Insert, Side::Before) => {
            return None;
        }
    };
    let span = diff.texts()[side].span(Range::from(line_idx..line_idx + 1));
    let text = diff.sources()[side][span.start..span.end].trim_end_matches(['\n', '\r']);
    let syn_spans = syntax[side]
        .as_ref()
        .and_then(|lines| lines.get(line_idx.saturating_sub(window[side].start)))
        .map(Vec::as_slice);
    let spans = content_spans(item.kind, text, syn_spans, &item.emphasis, ctx);
    Some(SplitCell {
        line_num: Some(line_idx + first_line),
        kind: item.kind,
        spans,
        _marker: std::marker::PhantomData,
    })
}

fn format_half(
    cell: Option<SplitCell<'_>>,
    width: usize,
    gutter_w: usize,
    ctx: &DiffRenderStyleContext,
) -> Vec<RtSpan<'static>> {
    let Some(cell) = cell else {
        let placeholder = format!(" {:>gutter_w$} ·", "");
        let used = UnicodeWidthStr::width(placeholder.as_str());
        let pad = width.saturating_sub(used);
        return vec![
            RtSpan::styled(placeholder, style_gutter(ctx)),
            RtSpan::raw(" ".repeat(pad)),
        ];
    };
    let (sign, sign_style, bg_opt) = match cell.kind {
        DiffLineType::Insert => ('+', style_sign(ctx, true), ctx.add_bg),
        DiffLineType::Delete => ('-', style_sign(ctx, false), ctx.del_bg),
        DiffLineType::Context => (' ', style_context(), None),
    };
    let bg_style = bg_opt.map(|bg| Style::default().bg(bg)).unwrap_or_default();
    let num_str = cell
        .line_num
        .map(|n| format!("{n:>gutter_w$} "))
        .unwrap_or_else(|| format!("{:gutter_w$} ", ""));
    let gutter_style = style_gutter(ctx).patch(bg_style);
    let sign_style = sign_style.patch(bg_style);

    let mut out = vec![
        RtSpan::styled(num_str.clone(), gutter_style),
        RtSpan::styled(format!("{sign} "), sign_style),
    ];
    let prefix_w = UnicodeWidthStr::width(num_str.as_str()) + 2;
    let content_budget = width.saturating_sub(prefix_w);
    let (clipped, clipped_w) = clip_spans(&cell.spans, content_budget, bg_style, style_gutter(ctx));
    out.extend(clipped);
    let rem = width.saturating_sub(prefix_w + clipped_w);
    if rem > 0 {
        out.push(RtSpan::styled(" ".repeat(rem), bg_style));
    }
    out
}

fn clip_spans(
    spans: &[RtSpan<'static>],
    max_w: usize,
    bg_style: Style,
    ellipsis: Style,
) -> (Vec<RtSpan<'static>>, usize) {
    if max_w == 0 {
        return (Vec::new(), 0);
    }
    let total_w: usize = spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    if total_w <= max_w {
        let styled = spans
            .iter()
            .map(|s| RtSpan::styled(s.content.clone(), bg_style.patch(s.style)))
            .collect();
        return (styled, total_w);
    }
    let limit = max_w.saturating_sub(usize::from(max_w >= 2));
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > limit {
                if !buf.is_empty() {
                    out.push(RtSpan::styled(buf, bg_style.patch(span.style)));
                }
                if max_w >= 2 {
                    out.push(RtSpan::styled("…", bg_style.patch(ellipsis)));
                    used += 1;
                }
                return (out, used);
            }
            buf.push(ch);
            used += cw;
        }
        if !buf.is_empty() {
            out.push(RtSpan::styled(buf, bg_style.patch(span.style)));
        }
    }
    (out, used)
}
