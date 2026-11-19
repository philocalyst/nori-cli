//! An edit's two texts diffed with disparate, and drawn as unified hunks.
//!
//! In a language disparate has a grammar for, the texts are diffed structurally
//! ([`Structural`]): code that was only re-indented is unchanged, code that was only rewrapped
//! has nothing new in it, and code that moved is not new where it went. The structural diff's
//! lines place the hunks, and its novel tokens are what is emphasised on any changed line.
//! Otherwise lines are diffed, and a changed line paired with the line it is an edit of
//! ([`View::rows_by`](disparate::View::rows_by), by [`EditPairing`]) has the words that differ
//! emphasised, compared with all the lines it became when it was rewrapped too. A line that is
//! new throughout is only tinted, as an added line is.
//!
//! Either way the cost is that of the changes: equal lines are compared as bytes and never
//! split or highlighted, only the windows around the changes are parsed, and only the lines
//! shown are highlighted.

use core::range::Range;
use std::path::Path;

use disparate::Both;
use disparate::Diff;
use disparate::Differ;
use disparate::Part;
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
use ratatui::text::Line as RtLine;
use ratatui::text::Span as RtSpan;

use super::highlighted_diff_lines_for_path;
use super::line::DiffLineLayout;
use super::line::DiffLineType;
use super::line::content_spans;
use super::line::diff_line_rows;
use super::line::line_number_width;
use super::style::DiffRenderStyleContext;
use super::style::style_gutter;

/// Unchanged lines shown either side of a hunk; the rest fold away, `⋮` between the hunks.
const CONTEXT_LINES: usize = 3;

/// The two texts of an edit and their diff: the lines that changed and, for a structural
/// diff, the novel bytes of each side.
pub(super) struct EditDiff<'a> {
    sources: Both<&'a str>,
    texts: Both<Text<&'a [u8]>>,
    lines: Diff,
    /// The novel byte ranges of each side, sorted and disjoint, when the texts were diffed by
    /// their grammar.
    novel: Option<Both<Vec<Range<usize>>>>,
}

/// A line of an edit as it is shown.
pub(super) struct Shown {
    pub(super) kind: DiffLineType,
    /// Its index in its own side's text: the old text for a removed line, else the new.
    pub(super) line: usize,
    /// The before-side line index (meaningful for `Context` lines in split diff).
    pub(super) before_line: usize,
    /// The byte ranges of the line, from its start, to emphasise.
    pub(super) emphasis: Vec<Range<usize>>,
}

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
            .map(|diff| {
                let novel = Both::new(
                    diff.novel(Side::Before).collect(),
                    diff.novel(Side::After).collect(),
                );
                (diff.lines(), novel)
            })
            // Reading a diff through texts of other lengths panics; never risk it on the UI
            // thread.
            .filter(|(lines, _)| lines.len() == lens);
        let (lines, novel) = match structural {
            Some((lines, novel)) => (lines, Some(novel)),
            None => (Differ::new().diff_lines(bytes.before, bytes.after), None),
        };
        Self {
            sources,
            texts,
            lines,
            novel,
        }
    }

    /// How many lines were removed and added.
    pub(super) fn changed(&self) -> Both<usize> {
        self.lines.changed()
    }

    pub(super) fn sources(&self) -> Both<&'a str> {
        self.sources
    }

    pub(super) fn texts(&self) -> &Both<Text<&'a [u8]>> {
        &self.texts
    }

    /// Draws the hunks into `out` within `width`, numbered from `first_line` and highlighted
    /// as `path`'s language. Uses side-by-side split view when `split` is true.
    pub(super) fn render(
        &self,
        first_line: usize,
        path: &Path,
        width: usize,
        split: bool,
        ctx: &DiffRenderStyleContext,
        out: &mut Vec<RtLine<'static>>,
    ) {
        let groups = self.shown();
        if split {
            super::split::render_split(self, &groups, first_line, path, width, ctx, out);
            return;
        }
        let side_of = |kind: DiffLineType| match kind {
            DiffLineType::Delete => Side::Before,
            DiffLineType::Insert | DiffLineType::Context => Side::After,
        };
        let max_line_number = groups
            .iter()
            .flat_map(|(_, shown)| shown)
            .map(|shown| shown.line + first_line)
            .max()
            .unwrap_or(0);
        let layout = DiffLineLayout {
            width,
            line_number_width: line_number_width(max_line_number),
        };
        for (index, (window, shown)) in groups.iter().enumerate() {
            if index > 0 {
                let spacer = format!("{:width$} ", "", width = layout.line_number_width.max(1));
                out.push(RtLine::from(vec![
                    RtSpan::styled(spacer, style_gutter(ctx)),
                    RtSpan::styled("⋮", style_gutter(ctx)),
                ]));
            }
            // Each side's lines in the window are highlighted together, in their own text,
            // and only when some of them are shown.
            let syntax = Both::new(Side::Before, Side::After).map(|side| {
                let used = shown.iter().any(|line| side_of(line.kind) == side);
                let span = self.texts[side].span(window[side]);
                used.then(|| &self.sources[side][span.start..span.end])
                    .and_then(|lines| highlighted_diff_lines_for_path(lines, path))
            });
            for line in shown {
                let side = side_of(line.kind);
                let span = self.texts[side].span(Range::from(line.line..line.line + 1));
                let text = self.sources[side][span.start..span.end].trim_end_matches(['\n', '\r']);
                let spans = syntax[side]
                    .as_ref()
                    .and_then(|lines| lines.get(line.line - window[side].start))
                    .map(Vec::as_slice);
                let content = content_spans(line.kind, text, spans, &line.emphasis, ctx);
                out.extend(diff_line_rows(
                    line.line + first_line,
                    line.kind,
                    content,
                    layout,
                    ctx,
                ));
            }
        }
    }

    /// The lines each hunk shows, with the window of lines it spans: unchanged lines as
    /// context, its removed lines before its added ones, as a unified patch has them.
    fn shown(&self) -> Vec<(Both<Range<usize>>, Vec<Shown>)> {
        let view = self.lines.on(&self.texts.before, &self.texts.after);
        let mut words = Refiner::new(Words);
        view.groups(CONTEXT_LINES)
            .map(|group| {
                let rows: Vec<Part<'_>> = group.rows_by(EditPairing::default()).collect();
                let mut emphasis = Vec::with_capacity(rows.len());
                let mut index = 0;
                while index < rows.len() {
                    let (row_emphasis, rewrapped) =
                        self.emphasis(&rows[index], &rows[index + 1..], &mut words);
                    emphasis.push(row_emphasis);
                    // The lines an edited line was rewrapped into have nothing new in them.
                    emphasis.extend((0..rewrapped).map(|_| Both::default()));
                    index += 1 + rewrapped;
                }
                let mut shown = Vec::new();
                // Added lines wait for the end of their hunk, below the removed ones.
                let mut added = Vec::new();
                for (row, mut emphasis) in rows.iter().zip(emphasis) {
                    let lines = row.tokens();
                    if !row.is_changed() {
                        shown.append(&mut added);
                        shown.push(Shown {
                            kind: DiffLineType::Context,
                            line: lines.after.start,
                            before_line: lines.before.start,
                            emphasis: Vec::new(),
                        });
                        continue;
                    }
                    if !lines.before.is_empty() {
                        shown.push(Shown {
                            kind: DiffLineType::Delete,
                            line: lines.before.start,
                            before_line: lines.before.start,
                            emphasis: std::mem::take(&mut emphasis.before),
                        });
                    }
                    if !lines.after.is_empty() {
                        added.push(Shown {
                            kind: DiffLineType::Insert,
                            line: lines.after.start,
                            before_line: lines.before.start,
                            emphasis: emphasis.after,
                        });
                    }
                }
                shown.append(&mut added);
                (group.window(), shown)
            })
            .collect()
    }

    /// What changed in each line of a changed row, as byte ranges from the start of the line,
    /// blanks at their ends left out: the novel tokens on it when the diff is structural, else,
    /// when the row pairs a line with the line it is an edit of, the words that differ between
    /// them. A line that is new throughout is left to its tint, as a line that was only added.
    ///
    /// A line edited and rewrapped at once (split in two, or joined with the next) is compared
    /// with all it became: the lone lines of one side that `following` starts with. When those
    /// have nothing new in them, the words that only moved onto them are not changed, and the
    /// second value is how many of `following` the row so accounted for; their emphasis is
    /// empty.
    fn emphasis(
        &self,
        row: &Part<'_>,
        following: &[Part<'_>],
        words: &mut Refiner<Words>,
    ) -> (Both<Vec<Range<usize>>>, usize) {
        let (lines, span) = (row.tokens(), row.span());
        let mut rewrapped = 0;
        let changed: Both<Vec<Range<usize>>> = match &self.novel {
            Some(novel) => novel.as_ref().zip(span).map(|(novel, line)| {
                let from = novel.partition_point(|r| r.end <= line.start);
                novel[from..]
                    .iter()
                    .take_while(|r| r.start < line.end)
                    .map(|r| Range::from(r.start.max(line.start)..r.end.min(line.end)))
                    .collect()
            }),
            None if !lines.before.is_empty() && !lines.after.is_empty() => {
                // The side that lone lines go on, if the next row is one.
                let side = following.first().and_then(|next| {
                    let next = next.tokens();
                    match (next.before.is_empty(), next.after.is_empty()) {
                        (true, false) => Some(Side::After),
                        (false, true) => Some(Side::Before),
                        (true, true) | (false, false) => None,
                    }
                });
                let merged = side.and_then(|side| {
                    let other = match side {
                        Side::Before => Side::After,
                        Side::After => Side::Before,
                    };
                    let run = following
                        .iter()
                        .take_while(|next| {
                            next.is_changed()
                                && next.tokens()[other].is_empty()
                                && !next.tokens()[side].is_empty()
                        })
                        .count();
                    let mut whole = span;
                    whole[side] =
                        Range::from(span[side].start..following[run - 1].span()[side].end);
                    let bytes = whole
                        .zip(self.texts.as_ref())
                        .map(|(whole, text)| &text.bytes()[whole]);
                    let changed = words.highlight(bytes.before, bytes.after).zip(whole).map(
                        |(ranges, whole)| {
                            ranges
                                .into_iter()
                                .map(|r| Range::from(r.start + whole.start..r.end + whole.start))
                                .collect::<Vec<_>>()
                        },
                    );
                    let lone = Range::from(span[side].end..whole[side].end);
                    let text = self.texts[side].bytes();
                    let nothing_new = changed[side].iter().all(|r| {
                        let (start, end) = (r.start.max(lone.start), r.end.min(lone.end));
                        start >= end || text[start..end].iter().all(u8::is_ascii_whitespace)
                    });
                    nothing_new.then(|| {
                        rewrapped = run;
                        changed
                    })
                });
                merged.unwrap_or_else(|| {
                    let refined = words.refine(row);
                    Both::new(
                        refined.changed(Side::Before).collect(),
                        refined.changed(Side::After).collect(),
                    )
                })
            }
            None => Both::default(),
        };
        let emphasis = changed
            .zip(span)
            .zip(self.texts.as_ref())
            .map(|((ranges, line), text)| {
                let bytes = &text.bytes()[line];
                // A novel run joined across lines takes in the line break and indent between.
                let trim = |mut start: usize, mut end: usize| {
                    while start < end && bytes[start].is_ascii_whitespace() {
                        start += 1;
                    }
                    while end > start && bytes[end - 1].is_ascii_whitespace() {
                        end -= 1;
                    }
                    (start < end).then(|| Range::from(start..end))
                };
                // Ranges are cut to the line: a rewrapped line's reach past it.
                let ranges: Vec<Range<usize>> = ranges
                    .into_iter()
                    .filter_map(|r| {
                        trim(
                            r.start.max(line.start) - line.start,
                            r.end.min(line.end).saturating_sub(line.start),
                        )
                    })
                    .collect();
                if ranges.len() == 1 && ranges.first().copied() == trim(0, bytes.len()) {
                    Vec::new()
                } else {
                    ranges
                }
            });
        (emphasis, rewrapped)
    }
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

impl Similarity for EditPairing {
    type Line<'a> = (Profile<'a>, Option<&'a [u8]>);

    fn line<'a>(&mut self, bytes: &'a [u8]) -> Self::Line<'a> {
        // The first token: a run of word characters, or the first other character.
        let head = bytes
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())
            .map(|start| {
                let rest = &bytes[start..];
                let word = rest
                    .iter()
                    .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                    .count();
                &rest[..word.max(1)]
            });
        (self.0.line(bytes), head)
    }

    fn score(&mut self, a: &Self::Line<'_>, b: &Self::Line<'_>) -> f32 {
        let score = self.0.score(&a.0, &b.0);
        if score >= THRESHOLD && a.1.is_some() && a.1 == b.1 {
            (score + SAME_HEAD).min(1.0)
        } else {
            score
        }
    }

    fn terms(&mut self, line: &Self::Line<'_>, terms: &mut Vec<Term>) {
        self.0.terms(&line.0, terms);
    }
}
