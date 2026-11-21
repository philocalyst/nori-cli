//! An edit's two texts diffed with disparate, and drawn the way a document editor shows
//! suggestions: one column of the new text, with every edited line merged in place.
//!
//! In a language disparate has a grammar for, the texts are diffed structurally
//! ([`Structural`]): code that was only re-indented is unchanged, and code that moved is not new
//! where it went. Otherwise lines are diffed. Either way a changed line is paired with the line it
//! is an edit of ([`View::rows_by`](disparate::View::rows_by), by [`EditPairing`]), and the pair is
//! diffed again by words: the words the line lost are struck through where they stood, the words
//! it gained are marked, and the rest reads as it was. A line rewritten more than it was kept
//! reads better as what it was and what it became, so it is drawn as both. A line that only
//! appeared is marked whole, one that only went is struck through whole.
//!
//! The cost is that of the changes: equal lines are compared as bytes and never split, only the
//! windows around the changes are parsed, and only the lines shown are highlighted.

use core::range::Range;
use std::path::Path;

use disparate::Both;
use disparate::Diff;
use disparate::Differ;
use disparate::Refiner;
use disparate::Side;
use disparate::Text;
use disparate::Words;
use disparate::pair::Dice;
use disparate::pair::Profile;
use disparate::pair::Similarity;
use disparate::pair::THRESHOLD;
use disparate::pair::Term;
use disparate::syntax::Language;
use disparate::syntax::Structural;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;

use super::highlighted_diff_lines_for_path;
use super::line::DiffLineLayout;
use super::line::Mark;
use super::line::diff_line_rows;
use super::line::line_number_width;
use super::style::DiffRenderStyleContext;
use super::style::style_gutter;

/// Unchanged lines shown either side of a hunk; the rest fold away, `⋮` between the hunks.
const CONTEXT_LINES: usize = 3;

/// An edited line is merged when what it kept (counting the bytes that are not blank) is most of
/// what it was and a fair part of what it became. One rewritten more, or spread over several
/// lines, is drawn as what it was and what it became.
const KEPT_OF_OLD: f32 = 0.6;
const KEPT_OF_NEW: f32 = 0.4;

/// The two texts of an edit and the lines that changed between them.
pub(super) struct EditDiff<'a> {
    sources: Both<&'a str>,
    texts: Both<Text<&'a [u8]>>,
    lines: Diff,
}

/// A line's highlighter colours, as the byte ranges they cover.
type Classes = Vec<(Range<usize>, Style)>;

impl<'a> EditDiff<'a> {
    /// The diff of `old_text` and `new_text`, read through the grammar `path` names when
    /// disparate has it and both texts parse there.
    pub(super) fn new(old_text: &'a str, new_text: &'a str, path: &Path) -> Self {
        let sources = Both::new(old_text, new_text);
        let bytes = sources.map(str::as_bytes);
        let texts = bytes.map(Text::lines);
        let lens = texts.as_ref().map(Text::len);
        // Code that does not parse is diffed as lines: tree-sitter's error recovery is not
        // the code.
        let structural = Language::from_path(path)
            .and_then(|language| Structural::parse(language, bytes.before, bytes.after))
            .filter(|diff| !diff.has_errors())
            .map(|diff| diff.lines())
            // Reading a diff through texts of other lengths panics; never risk it on the UI
            // thread.
            .filter(|lines| lines.len() == lens);
        let lines =
            structural.unwrap_or_else(|| Differ::new().diff_lines(bytes.before, bytes.after));
        Self {
            sources,
            texts,
            lines,
        }
    }

    /// How many lines were removed and added.
    pub(super) fn changed(&self) -> Both<usize> {
        self.lines.changed()
    }

    /// Draws the hunks into `out` within `width`, numbered from `first_line` and highlighted
    /// as `path`'s language.
    pub(super) fn render(
        &self,
        first_line: usize,
        path: &Path,
        width: usize,
        ctx: &DiffRenderStyleContext,
        out: &mut Vec<RtLine<'static>>,
    ) {
        let view = self.lines.on(&self.texts.before, &self.texts.after);
        let groups: Vec<_> = view.groups(CONTEXT_LINES).collect();
        let last_line = groups
            .last()
            .map_or(0, |group| group.window().after.end + first_line);
        let layout = DiffLineLayout {
            width,
            line_number_width: line_number_width(last_line),
        };
        let mut words = Refiner::new(Words);
        for (index, group) in groups.into_iter().enumerate() {
            if index > 0 {
                let spacer = format!("{:width$} ", "", width = layout.line_number_width.max(1));
                out.push(RtLine::from(vec![
                    RtSpan::styled(spacer, style_gutter(ctx)),
                    RtSpan::styled("⋮", style_gutter(ctx)),
                ]));
            }
            let window = group.window();
            // Each side's lines in the window are highlighted together, in their own text.
            let syntax = Both::new(Side::Before, Side::After).map(|side| {
                let span = self.texts[side].span(window[side]);
                highlighted_diff_lines_for_path(&self.sources[side][span.start..span.end], path)
            });
            let classes = |side: Side, line: usize| -> Classes {
                let mut at = 0;
                syntax[side]
                    .as_ref()
                    .and_then(|lines| lines.get(line - window[side].start))
                    .map_or_else(Vec::new, |spans| {
                        spans
                            .iter()
                            .map(|span| {
                                let start = at;
                                at += span.content.len();
                                (Range::from(start..at), span.style)
                            })
                            .collect()
                    })
            };
            // A run of changed lines is drawn as a unified diff orders it: what went (and was not
            // merged into a line that stayed) first, then the lines as they are now, edited ones
            // merged in place.
            let mut gone: Vec<RtLine<'static>> = Vec::new();
            let mut now: Vec<RtLine<'static>> = Vec::new();
            let flush = |gone: &mut Vec<RtLine<'static>>,
                         now: &mut Vec<RtLine<'static>>,
                         out: &mut Vec<RtLine<'static>>| {
                out.append(gone);
                out.append(now);
            };
            for row in group.rows_by(EditPairing::default()) {
                let lines = row.tokens();
                let (before, after) = (lines.before.start, lines.after.start);
                let number = Some(after + first_line);
                if !row.is_changed() {
                    flush(&mut gone, &mut now, out);
                    let text = self.line(Side::After, after);
                    let content = spans(text, &classes(Side::After, after), &[], Style::default());
                    out.extend(diff_line_rows(number, Mark::Kept, content, layout, ctx));
                    continue;
                }
                let went = |gone: &mut Vec<RtLine<'static>>| {
                    let text = self.line(Side::Before, before);
                    let content = removed(text, &classes(Side::Before, before), ctx);
                    gone.extend(diff_line_rows(None, Mark::Removed, content, layout, ctx));
                };
                let came = |now: &mut Vec<RtLine<'static>>| {
                    let text = self.line(Side::After, after);
                    let content = added(text, &classes(Side::After, after), ctx);
                    now.extend(diff_line_rows(number, Mark::Added, content, layout, ctx));
                };
                match (lines.before.is_empty(), lines.after.is_empty()) {
                    (false, false) => {
                        let texts = Both::new(
                            self.line(Side::Before, before),
                            self.line(Side::After, after),
                        );
                        match merge(&mut words, texts, &classes(Side::After, after), ctx) {
                            Some(content) => now.extend(diff_line_rows(
                                number,
                                Mark::Edited,
                                content,
                                layout,
                                ctx,
                            )),
                            None => {
                                went(&mut gone);
                                came(&mut now);
                            }
                        }
                    }
                    (false, true) => went(&mut gone),
                    (true, _) => came(&mut now),
                }
            }
            flush(&mut gone, &mut now, out);
        }
    }

    /// Line `line` of `side`'s text, without its line break.
    fn line(&self, side: Side, line: usize) -> &'a str {
        let span = self.texts[side].span(Range::from(line..line + 1));
        self.sources[side][span.start..span.end].trim_end_matches(['\n', '\r'])
    }
}

/// `text` drawn in its highlighter colours (`classes`, else the default colour), with the byte
/// ranges `marked` (sorted, disjoint) patched with `mark`.
fn spans(
    text: &str,
    classes: &Classes,
    marked: &[Range<usize>],
    mark: Style,
) -> Vec<RtSpan<'static>> {
    disparate::layout::segments(Range::from(0..text.len()), classes, marked)
        .map(|segment| {
            let style = segment.class.unwrap_or_default();
            let style = if segment.changed {
                style.patch(mark)
            } else {
                style
            };
            RtSpan::styled(slice(text, segment.range), style)
        })
        .collect()
}

/// The bytes `range` of `text`, as text: the word diff cuts at bytes.
fn slice(text: &str, range: Range<usize>) -> String {
    String::from_utf8_lossy(&text.as_bytes()[range.start..range.end]).into_owned()
}

/// The bytes of `text` from its first that is not blank: what marking a whole line covers, so
/// its indent stays clear.
fn from_indent(text: &str) -> Vec<Range<usize>> {
    let start = text.len() - text.trim_start().len();
    if start < text.len() {
        vec![Range::from(start..text.len())]
    } else {
        Vec::new()
    }
}

/// A line that only appeared: marked whole.
fn added(text: &str, classes: &Classes, ctx: &DiffRenderStyleContext) -> Vec<RtSpan<'static>> {
    spans(text, classes, &from_indent(text), ctx.added())
}

/// A line that only went: struck through whole, its colours dimmed.
fn removed(text: &str, classes: &Classes, ctx: &DiffRenderStyleContext) -> Vec<RtSpan<'static>> {
    let dimmed: Classes = classes
        .iter()
        .map(|(range, style)| (*range, style.add_modifier(Modifier::DIM)))
        .collect();
    spans(text, &dimmed, &from_indent(text), ctx.removed())
}

/// An edited line merged in place: the new line in its colours, the words it gained marked, and
/// the words it lost struck through where they stood, before what replaced them. `None` when it
/// kept too little of itself to read as one line.
fn merge(
    words: &mut Refiner<Words>,
    texts: Both<&str>,
    classes: &Classes,
    ctx: &DiffRenderStyleContext,
) -> Option<Vec<RtSpan<'static>>> {
    let view = words.refine_bytes(texts.map(str::as_bytes));
    let solid = |bytes: &[u8]| {
        bytes
            .iter()
            .filter(|byte| !byte.is_ascii_whitespace())
            .count()
    };
    let mut kept = 0;
    let mut gained = Vec::new();
    // Where in the new line each lost run stood, and the run in the old line.
    let mut lost: Vec<(usize, Range<usize>)> = Vec::new();
    for part in view.parts() {
        let span = part.span();
        if !part.is_changed() {
            kept += solid(&texts.after.as_bytes()[span.after]);
            continue;
        }
        if !texts.before[span.before.start..span.before.end]
            .trim()
            .is_empty()
        {
            lost.push((span.after.start, span.before));
        }
        // Blanks gained (an indent that changed) are not worth marking.
        if !texts.after[span.after.start..span.after.end]
            .trim()
            .is_empty()
        {
            gained.push(span.after);
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let share = |of: &str| kept as f32 / solid(of.as_bytes()).max(1) as f32;
    if share(texts.before) < KEPT_OF_OLD || share(texts.after) < KEPT_OF_NEW {
        return None;
    }
    let text = texts.after;
    let mut out = Vec::new();
    let mut lost = lost.into_iter().peekable();
    for segment in disparate::layout::segments(Range::from(0..text.len()), classes, &gained) {
        let style = segment.class.unwrap_or_default();
        let style = if segment.changed {
            style.patch(ctx.gained())
        } else {
            style
        };
        let mut at = segment.range.start;
        // A lost run that stood inside this segment cuts it there.
        while let Some((cut, run)) = lost.next_if(|(cut, _)| *cut < segment.range.end) {
            if cut > at {
                out.push(RtSpan::styled(slice(text, Range::from(at..cut)), style));
                at = cut;
            }
            out.push(RtSpan::styled(slice(texts.before, run), ctx.lost()));
        }
        if at < segment.range.end {
            out.push(RtSpan::styled(
                slice(text, Range::from(at..segment.range.end)),
                style,
            ));
        }
    }
    // What the line lost at its very end.
    for (_, run) in lost {
        out.push(RtSpan::styled(slice(texts.before, run), ctx.lost()));
    }
    Some(out)
}

/// Pairs a changed line with the line it is an edit of, for a line diff.
///
/// [`Dice`] weighs what two lines share against the sum of their sizes, so when an edit also
/// splits a line in two, the shorter half can outscore the half that kept the line's head, which
/// is where its words were edited (`let total = area(3, 4) + perimeter(3, 4);` became
/// `let sum = area(3, 4)` and `+ perimeter(3, 4);`). An edited line keeps its head: the keyword
/// or name it starts with. So lines that start with the same token score a little more, enough
/// to settle such a near tie and never enough to pair lines that Dice keeps apart.
#[derive(Clone, Default)]
struct EditPairing(Dice);

/// What starting with the same token adds to a pairing's score.
const SAME_HEAD: f32 = 0.1;

/// What starting with the same token and then the same name adds (`fn area(` and `fn area(`, not
/// `fn perimeter(`): a definition keeps its name when it is edited.
const SAME_NAME: f32 = 0.2;

/// A line's first two tokens: each a run of word characters or one other character, blanks
/// between them skipped.
fn heads(bytes: &[u8]) -> [Option<&[u8]>; 2] {
    let token = |from: usize| -> Option<(&[u8], usize)> {
        let start = from
            + bytes[from..]
                .iter()
                .position(|b| !b.is_ascii_whitespace())?;
        let rest = &bytes[start..];
        let word = rest
            .iter()
            .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
            .count()
            .max(1);
        Some((&rest[..word], start + word))
    };
    let first = token(0);
    let second = first.and_then(|(_, end)| token(end));
    [
        first.map(|(token, _)| token),
        second.map(|(token, _)| token),
    ]
}

impl Similarity for EditPairing {
    type Line<'a> = (Profile<'a>, [Option<&'a [u8]>; 2]);

    fn line<'a>(&mut self, bytes: &'a [u8]) -> Self::Line<'a> {
        (self.0.line(bytes), heads(bytes))
    }

    fn score(&mut self, a: &Self::Line<'_>, b: &Self::Line<'_>) -> f32 {
        let score = self.0.score(&a.0, &b.0);
        let same = |index: usize| a.1[index].is_some() && a.1[index] == b.1[index];
        if score < THRESHOLD || !same(0) {
            score
        } else if same(1) {
            (score + SAME_NAME).min(1.0)
        } else {
            (score + SAME_HEAD).min(1.0)
        }
    }

    fn terms(&mut self, line: &Self::Line<'_>, terms: &mut Vec<Term>) {
        self.0.terms(&line.0, terms);
    }
}
