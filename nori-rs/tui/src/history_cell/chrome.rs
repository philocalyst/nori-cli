//! The design system every transcript cell is drawn from.
//!
//! A cell never names a colour. It asks [`Chrome`] for a token (`text_strong`, `muted`,
//! `accent_mint`, a well, a card) and for the few shapes the transcript is made of:
//!
//! * a **gutter**: column 0 holds a status glyph, text starts at column 2, so tool headers,
//!   prose, notices and subagent lanes all hang from the same two edges;
//! * a **header**: `● Verb target · meta`, always one line, truncated rather than wrapped;
//! * a **body**: rows indented under the header, on the code well when the terminal can show
//!   one and behind a faint rail when it cannot;
//! * **chips** for the one fact that must not be missed (`exit 2`, `queued`).
//!
//! Nothing here repeats information the header already carries, and nothing draws a row for a
//! fact that is not worth one: a duration under a second, an exit code of zero and an empty
//! output are all absent rather than drawn quietly.

use std::time::Duration;
use std::time::Instant;

use nori_tui_components::Theme;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

/// Columns the status glyph and its space take; text hangs from here.
pub(crate) const GUTTER: usize = 2;

pub(crate) const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Agents that serve other companies' models: the model alone does not say where it ran.
const GATEWAYS: [&str; 3] = ["opencode", "openrouter", "kilo"];

pub(crate) const SPINNER_PERIOD: Duration = Duration::from_millis(80);

/// Where a unit of work is, which decides its glyph and colour everywhere it is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    Running,
    /// Waiting on someone: an approval, a queue.
    Waiting,
    Done,
    Failed,
    Cancelled,
    /// Alive but with nothing to do.
    Idle,
}

/// How much of a cell is drawn: the line that says what happened, or everything that supports
/// it. Collapsed is the default; focus and `Alt+E` expand.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum Disclosure {
    #[default]
    Collapsed,
    Expanded,
}

/// A palette view for one render: the live theme and whether it can paint backgrounds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Chrome {
    pub(crate) theme: Theme,
}

#[cfg(feature = "storybook")]
thread_local! {
    /// The theme specimens are drawn with, instead of the terminal's.
    static THEME_OVERRIDE: std::cell::Cell<Option<Theme>> = const { std::cell::Cell::new(None) };
}

/// The theme everything in the transcript is drawn with: the terminal's, or (in specimens) the
/// one a specimen asked for.
pub(crate) fn live_theme() -> Theme {
    #[cfg(feature = "storybook")]
    if let Some(theme) = THEME_OVERRIDE.with(std::cell::Cell::get) {
        return theme;
    }
    crate::style::component_theme()
}

/// Draw with `theme` while `draw` runs, for specimens of other terminals.
#[cfg(feature = "storybook")]
pub(crate) fn with_theme<R>(theme: Theme, draw: impl FnOnce() -> R) -> R {
    let previous = THEME_OVERRIDE.with(|cell| cell.replace(Some(theme)));
    let drawn = draw();
    THEME_OVERRIDE.with(|cell| cell.set(previous));
    drawn
}

impl Chrome {
    pub(crate) fn live() -> Self {
        Self {
            theme: live_theme(),
        }
    }

    /// A model (and the agent it ran on) as an identity: the model's short name in the style of
    /// the provider whose model it is. The agent is named only when the model alone does not say
    /// where it ran: a model served through a gateway (`deepseek/v4 via opencode`) or no model
    /// at all.
    pub(crate) fn identity(&self, agent: Option<&str>, model: Option<&str>) -> Vec<Span<'static>> {
        let model = model.map(short_model).filter(|model| !model.is_empty());
        let model = model.as_deref();
        let agent = agent.filter(|agent| !agent.is_empty());
        let family = |text: &str| -> Option<Style> {
            let text = text.to_ascii_lowercase();
            let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
            if has(&["claude", "sonnet", "opus", "haiku"]) {
                Some(self.theme.provider_claude)
            } else if has(&["gpt", "codex", "o3", "o4"]) {
                Some(self.theme.provider_codex)
            } else if has(&["gemini"]) {
                Some(self.theme.provider_gemini)
            } else if has(&["antigravity", "carbon", "hydrogen", "argon"]) {
                Some(self.theme.provider_antigravity)
            } else if has(&["nori"]) {
                Some(self.theme.provider_nori)
            } else {
                None
            }
        };
        let gateway = agent.filter(|agent| GATEWAYS.contains(agent));
        match (agent, model) {
            (_, Some(model)) => {
                let style = family(model)
                    .or_else(|| agent.and_then(family))
                    .unwrap_or_else(|| self.muted());
                let mut spans = vec![Span::styled(model.to_owned(), style)];
                if let Some(gateway) = gateway {
                    spans.push(Span::styled(format!(" via {gateway}"), self.faint()));
                }
                spans
            }
            (Some(agent), None) => {
                let style = family(agent).unwrap_or_else(|| self.muted());
                vec![Span::styled(agent.to_owned(), style)]
            }
            (None, None) => Vec::new(),
        }
    }

    /// Whether wells and cards are painted. Without a known terminal background the surfaces
    /// are unset and structure comes from rails instead.
    pub(crate) fn has_surfaces(&self) -> bool {
        self.theme.code_well.bg.is_some()
    }

    pub(crate) fn strong(&self) -> Style {
        self.theme.text_strong
    }

    pub(crate) fn secondary(&self) -> Style {
        self.theme.text_secondary
    }

    pub(crate) fn muted(&self) -> Style {
        self.theme.muted
    }

    pub(crate) fn faint(&self) -> Style {
        self.theme.faint
    }

    pub(crate) fn mint(&self) -> Style {
        self.theme.accent_mint
    }

    pub(crate) fn cyan(&self) -> Style {
        self.theme.accent_cyan
    }

    pub(crate) fn violet(&self) -> Style {
        self.theme.accent_violet
    }

    pub(crate) fn amber(&self) -> Style {
        self.theme.accent_amber
    }

    pub(crate) fn coral(&self) -> Style {
        self.theme.accent_coral
    }

    /// The colour a [`Progress`] is drawn in.
    pub(crate) fn tone(&self, progress: Progress) -> Style {
        match progress {
            Progress::Running | Progress::Waiting => self.amber(),
            Progress::Done => self.mint(),
            Progress::Failed => self.coral(),
            Progress::Cancelled => self.muted(),
            Progress::Idle => self.faint(),
        }
    }

    /// The status glyph for a unit of work. `since` drives the spinner while it runs.
    pub(crate) fn glyph(
        &self,
        progress: Progress,
        since: Option<Instant>,
        animate: bool,
    ) -> Span<'static> {
        let text = match progress {
            Progress::Running if animate => {
                let phase = since.map_or(0, |started| {
                    started.elapsed().as_millis() / SPINNER_PERIOD.as_millis()
                });
                SPINNER_FRAMES[usize::try_from(phase % 10).unwrap_or(0)]
            }
            Progress::Running | Progress::Done | Progress::Failed => "●",
            Progress::Waiting => "◌",
            Progress::Cancelled => "⊘",
            Progress::Idle => "○",
        };
        Span::styled(text, self.tone(progress))
    }

    /// A quiet fact of a header.
    pub(crate) fn fact(&self, text: impl Into<String>) -> Span<'static> {
        Span::styled(text.into(), self.muted())
    }

    /// A separator between facts of a header: faint, so facts read as a list.
    pub(crate) fn dot(&self) -> Span<'static> {
        Span::styled(" · ", self.faint())
    }

    /// `+14 −4`: lines added and removed.
    pub(crate) fn line_counts(&self, added: usize, removed: usize) -> Vec<Span<'static>> {
        vec![
            Span::styled(format!("+{added}"), self.mint()),
            Span::raw(" "),
            Span::styled(format!("−{removed}"), self.coral()),
        ]
    }

    /// The row that stands for what a collapsed body left out.
    pub(crate) fn more_row(&self, hidden: usize, noun: &str) -> Line<'static> {
        Line::from(vec![
            self.body_prefix(),
            Span::styled(format!("⋯ +{hidden} more {noun}"), self.faint()),
        ])
    }

    /// What prefixes the rows of a body: two spaces on a well, a faint rail without one.
    pub(crate) fn body_prefix(&self) -> Span<'static> {
        if self.has_surfaces() {
            Span::raw("  ")
        } else {
            Span::styled("  │ ", self.faint())
        }
    }

    /// Columns [`Chrome::body_prefix`] takes.
    pub(crate) fn body_indent(&self) -> usize {
        if self.has_surfaces() { GUTTER } else { 4 }
    }

    /// Rows on the code well: indented, padded to `width` so the surface is a clean block.
    /// Without surfaces the rows keep only their rail.
    pub(crate) fn well(&self, rows: Vec<Vec<Span<'static>>>, width: usize) -> Vec<Line<'static>> {
        let surface = self.theme.code_well;
        rows.into_iter()
            .map(|mut spans| {
                spans.insert(0, self.body_prefix());
                if !self.has_surfaces() {
                    return Line::from(spans);
                }
                let used = spans_width(&spans);
                if used < width {
                    spans.push(Span::raw(" ".repeat(width - used)));
                }
                for span in &mut spans {
                    span.style = surface.patch(span.style);
                }
                Line::from(spans).style(surface)
            })
            .collect()
    }

    /// A header line: `● Verb target · meta`. It never wraps: the target gives way first (paths
    /// lose their head, anything else its tail), then the meta is dropped.
    pub(crate) fn header(&self, header: Header, width: usize) -> Line<'static> {
        let Header {
            glyph,
            verb,
            target,
            chip,
            meta,
        } = header;
        let mut spans = vec![glyph, Span::raw(" ")];
        let mut budget = width.saturating_sub(GUTTER);
        let verb_width = verb.width();
        let has_verb = !verb.is_empty();
        if has_verb {
            spans.push(Span::styled(verb, self.strong()));
            budget = budget.saturating_sub(verb_width);
        }
        let chip_width = if chip.is_empty() {
            0
        } else {
            1 + spans_width(&chip)
        };
        let meta_width: usize = meta.iter().map(|fact| 3 + fact.content.width()).sum();
        // Quiet facts never cost the subject anything: they are dropped before it is cut.
        let left_for_target = budget.saturating_sub(chip_width + meta_width + 1);
        let meta = if left_for_target >= target.width() {
            meta
        } else {
            Vec::new()
        };
        let meta_width: usize = meta.iter().map(|fact| 3 + fact.content.width()).sum();
        let room = budget.saturating_sub(chip_width + meta_width + 1);
        if !target.is_empty() {
            if has_verb {
                spans.push(Span::raw(" "));
            }
            spans.extend(target.fit(room, self));
        }
        if !chip.is_empty() {
            spans.push(Span::raw(" "));
            spans.extend(chip);
        }
        for fact in meta {
            spans.push(self.dot());
            spans.push(fact);
        }
        Line::from(spans)
    }
}

/// What a header says, before it is fitted to a width.
pub(crate) struct Header {
    pub(crate) glyph: Span<'static>,
    pub(crate) verb: String,
    pub(crate) target: Target,
    /// Loud facts that outrank meta: `+14 −4`, `exit 2`.
    pub(crate) chip: Vec<Span<'static>>,
    /// Quiet facts after the target, each already styled (see [`Chrome::fact`]).
    pub(crate) meta: Vec<Span<'static>>,
}

/// The subject of a header, which decides how it gives way to a narrow line.
pub(crate) enum Target {
    None,
    /// A path: its directory is faint, its file name stands out, the head is cut first.
    Path(String),
    /// Prose: cut from the end.
    Text(String),
    /// Already styled (a highlighted command): cut from the end.
    Spans(Vec<Span<'static>>),
}

impl Target {
    fn is_empty(&self) -> bool {
        match self {
            Self::None => true,
            Self::Path(text) | Self::Text(text) => text.is_empty(),
            Self::Spans(spans) => spans.is_empty(),
        }
    }

    fn width(&self) -> usize {
        match self {
            Self::None => 0,
            Self::Path(text) | Self::Text(text) => text.width(),
            Self::Spans(spans) => spans_width(spans),
        }
    }

    fn fit(self, room: usize, chrome: &Chrome) -> Vec<Span<'static>> {
        match self {
            Self::None => Vec::new(),
            Self::Path(path) => path_spans(&truncate_head(&path, room), chrome),
            Self::Text(text) => vec![Span::styled(truncate_tail(&text, room), chrome.secondary())],
            Self::Spans(spans) => truncate_spans(spans, room),
        }
    }
}

/// A path as spans: directory faint, file name (and any `:range`) in the normal text colour.
pub(crate) fn path_spans(path: &str, chrome: &Chrome) -> Vec<Span<'static>> {
    match path.rfind('/') {
        Some(slash) => vec![
            Span::styled(path[..=slash].to_owned(), chrome.faint()),
            Span::styled(path[slash + 1..].to_owned(), chrome.secondary()),
        ],
        None => vec![Span::styled(path.to_owned(), chrome.secondary())],
    }
}

pub(crate) fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| span.content.width()).sum()
}

/// `text` cut to `max` columns, ending in `…`.
pub(crate) fn truncate_tail(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for character in text.chars() {
        let columns = character.width().unwrap_or(0);
        if used + columns + 1 > max {
            break;
        }
        out.push(character);
        used += columns;
    }
    out.push('…');
    out
}

/// `text` cut to `max` columns from the front, starting with `…`; for paths, whose end says the
/// most.
pub(crate) fn truncate_head(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    if max == 0 {
        return String::new();
    }
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for character in text.chars().rev() {
        let columns = character.width().unwrap_or(0);
        if used + columns + 1 > max {
            break;
        }
        kept.push(character);
        used += columns;
    }
    kept.reverse();
    let kept: String = kept.into_iter().collect();
    // Cut at a directory boundary when there is one, so the path never starts mid-name.
    match kept.find('/') {
        Some(slash) if slash > 0 && slash + 1 < kept.len() => format!("…{}", &kept[slash..]),
        _ => format!("…{kept}"),
    }
}

/// A model id as every surface writes it: the one canonical label (`GPT-6 Luna`,
/// `Claude Opus 5.5`), without the vendor namespace.
pub(crate) fn short_model(model: &str) -> String {
    nori_harness::recovery::model_label(model, model, None)
}

/// `spans` cut to `max` columns, the last kept span ending in `…`.
pub(crate) fn truncate_spans(spans: Vec<Span<'static>>, max: usize) -> Vec<Span<'static>> {
    if spans_width(&spans) <= max {
        return spans;
    }
    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let columns = span.content.width();
        if used + columns < max {
            used += columns;
            out.push(span);
            continue;
        }
        let room = max.saturating_sub(used);
        out.push(Span::styled(
            truncate_tail(&span.content, room.max(1)),
            span.style,
        ));
        break;
    }
    out
}

/// One piece of a row with the separator that leads it, and how much it matters.
pub(crate) struct Segment {
    /// 0 is the subject, which is cut as a last resort; a higher rank is dropped sooner.
    pub(crate) rank: u8,
    pub(crate) spans: Vec<Span<'static>>,
}

impl Segment {
    pub(crate) fn new(rank: u8, spans: Vec<Span<'static>>) -> Self {
        Self { rank, spans }
    }
}

/// The segments that fit `width`, in their order. The least important go first, whole, with
/// their separators, so a narrow row never ends in a dangling `·` or half a word of a state;
/// only the subject itself is ever cut.
pub(crate) fn fit_segments(segments: Vec<Segment>, width: usize) -> Vec<Span<'static>> {
    let mut kept: Vec<Option<Segment>> = segments.into_iter().map(Some).collect();
    let total = |kept: &[Option<Segment>]| -> usize {
        kept.iter()
            .flatten()
            .map(|segment| spans_width(&segment.spans))
            .sum()
    };
    while total(&kept) > width {
        let droppable = kept
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| {
                segment
                    .as_ref()
                    .filter(|segment| segment.rank > 0)
                    .map(|segment| (segment.rank, index))
            })
            .max();
        let Some((_, index)) = droppable else {
            break;
        };
        kept[index] = None;
    }
    let spans: Vec<Span<'static>> = kept
        .into_iter()
        .flatten()
        .flat_map(|segment| segment.spans)
        .collect();
    truncate_spans(spans, width)
}

/// `1.4s`, `12s`, `1m 05s`; nothing under a second.
pub(crate) fn format_duration(elapsed: Duration) -> Option<String> {
    let millis = elapsed.as_millis();
    if millis < 1000 {
        return None;
    }
    Some(if millis < 10_000 {
        format!("{}.{}s", millis / 1000, millis % 1000 / 100)
    } else if millis < 60_000 {
        format!("{}s", millis / 1000)
    } else {
        format!("{}m {:02}s", millis / 60_000, millis / 1000 % 60)
    })
}

/// Lines laid side by side, each column `column_width` wide with `gap` columns between, padding
/// short columns with blank rows. The one primitive behind bento rows and parallel lanes.
pub(crate) fn side_by_side(
    columns: Vec<Vec<Line<'static>>>,
    column_width: usize,
    gap: usize,
) -> Vec<Line<'static>> {
    let rows = columns.iter().map(Vec::len).max().unwrap_or(0);
    let mut out = Vec::with_capacity(rows);
    for row in 0..rows {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                spans.push(Span::raw(" ".repeat(gap)));
            }
            let cell = column.get(row);
            let used = cell.map_or(0, |line| spans_width(&line.spans));
            if let Some(line) = cell {
                spans.extend(line.spans.iter().cloned().map(|mut span| {
                    span.style = line.style.patch(span.style);
                    span
                }));
            }
            if used < column_width {
                let pad = Span::raw(" ".repeat(column_width - used));
                spans.push(match cell {
                    Some(line) => Span::styled(pad.content, line.style),
                    None => pad,
                });
            }
        }
        out.push(Line::from(spans));
    }
    out
}

/// How many equal columns of at least `min_column` fit in `width` with `gap` between them,
/// at most `max`.
pub(crate) fn columns_for(width: usize, min_column: usize, gap: usize, max: usize) -> usize {
    (width.saturating_add(gap) / (min_column + gap).max(1)).clamp(1, max.max(1))
}
