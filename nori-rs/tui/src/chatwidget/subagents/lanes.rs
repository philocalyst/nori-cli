//! Running subagents, drawn in the chat as lanes of one tree under `◈ main`:
//!
//! ```text
//! ◈ main · 3 running
//! ├─ ⠹ auth-audit  claude/sonnet-4 · 12s · Read src/auth.rs
//! │  ⋮ 4 nested · ●2 ✓1 ✗1 · $ cargo test -p auth
//! ├─ ⠼ cfg-trace  codex/gpt-5 · 4s · Edit wake.rs
//! └─ ✗ reviewer  gemini/pro · usage limit
//! ```
//!
//! A subtree that is not focused is one row (how many are under it, by state, and what is
//! happening in it now), however deep it goes. Focus a subagent and the path to it opens, one row
//! per child, each child's own subtree compressed again. When the terminal is wide and several
//! children run in parallel, they sit side by side as cards instead of one under another.
//! Nothing here is ever below the composer, and nothing is a separate view.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use super::SPINNER_PERIOD;
use super::SubagentTree;
use super::node::Node;
use super::node::NodeId;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Progress;
use crate::history_cell::chrome::Segment;
use crate::history_cell::chrome::columns_for;
use crate::history_cell::chrome::fit_segments;
use crate::history_cell::chrome::format_duration;
use crate::history_cell::chrome::side_by_side;
use crate::history_cell::chrome::spans_width;
use crate::history_cell::chrome::truncate_spans;
use crate::history_cell::chrome::truncate_tail;
use crate::render::bash_highlight::highlight_bash_spans;
use crate::render::renderable::Renderable;
use crate::subagent_switcher::SubagentKey;

/// The width from which parallel children sit side by side.
const PARALLEL_MIN_WIDTH: usize = 100;
const LANE_MIN_WIDTH: usize = 48;
const LANE_GAP: usize = 1;
const MAX_LANE_COLUMNS: usize = 3;
/// The block takes at most a third of the screen, and never fewer than this many rows.
const MIN_ROWS: usize = 4;

/// How a set of subagents stands, by state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Counts {
    pub(super) running: usize,
    pub(super) idle: usize,
    pub(super) done: usize,
    pub(super) failed: usize,
    pub(super) cancelled: usize,
}

impl Counts {
    pub(super) fn add(&mut self, other: Counts) {
        self.running += other.running;
        self.idle += other.idle;
        self.done += other.done;
        self.failed += other.failed;
        self.cancelled += other.cancelled;
    }

    pub(super) fn total(&self) -> usize {
        self.running + self.idle + self.done + self.failed + self.cancelled
    }

    /// `●2 ✓1 ✗1`: the states that have anyone in them.
    pub(super) fn spans(&self, chrome: &Chrome) -> Vec<Span<'static>> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (count, glyph, style) in [
            (self.running, "●", chrome.amber()),
            (self.idle, "○", chrome.faint()),
            (self.done, "✓", chrome.mint()),
            (self.failed, "✗", chrome.coral()),
            (self.cancelled, "⊘", chrome.muted()),
        ] {
            if count > 0 {
                if !spans.is_empty() {
                    spans.push(Span::raw(" "));
                }
                spans.push(Span::styled(format!("{glyph}{count}"), style));
            }
        }
        spans
    }
}

/// What a closed subtree says: how many are under it, by state, and who is busiest.
#[derive(Clone, Copy, Debug, Default)]
struct Below {
    counts: Counts,
    latest: Option<NodeId>,
}

/// [`Below`] for the nodes asked about since the tree last changed.
#[derive(Debug, Default)]
pub(super) struct BelowMemo {
    revision: u64,
    nodes: std::collections::HashMap<NodeId, Below>,
}

/// The block as last laid out, and what it was laid out for. A frame that asks for the same
/// block again (the height, then the rows, then the next frame of a still screen) reuses it.
#[derive(Debug)]
pub(super) struct LaneCache {
    key: (u64, u16, u16, Option<SubagentKey>, u128),
    lines: Vec<Line<'static>>,
    hits: Vec<(Rect, SubagentKey)>,
}

pub(crate) struct InChatSubagents<'a> {
    pub(crate) tree: &'a SubagentTree,
    pub(crate) focused: Option<SubagentKey>,
    pub(crate) now: Instant,
    pub(crate) screen_rows: u16,
    pub(crate) hits: &'a RefCell<Vec<(Rect, SubagentKey)>>,
}

/// The block laid out at a width: its rows, and where each subagent can be clicked.
struct Laid {
    lines: Vec<Line<'static>>,
    hits: Vec<(Rect, SubagentKey)>,
}

impl InChatSubagents<'_> {
    fn budget(&self) -> usize {
        if self.screen_rows == 0 {
            usize::MAX
        } else {
            (usize::from(self.screen_rows) / 3).max(MIN_ROWS)
        }
    }

    /// Running subagents that no running subagent owns: the lanes' first level. A running child
    /// of an idle parent is one too, so it is never hidden behind the parent that rests.
    fn roots(&self) -> Vec<NodeId> {
        self.tree
            .live
            .iter()
            .filter(|(id, node)| !node.idle && !self.tree.has_running_ancestor(**id))
            .map(|(id, _)| *id)
            .collect()
    }

    /// The focused subagent and everything above it: the path that stays open.
    fn open_path(&self) -> BTreeSet<NodeId> {
        let mut path = BTreeSet::new();
        let mut at = self.focused.map(|key| NodeId(key.0));
        while let Some(id) = at {
            if !path.insert(id) {
                break;
            }
            at = self.tree.live.get(&id).and_then(|node| node.parent);
        }
        path
    }

    /// The states of everything under `id`, not counting `id` itself.
    fn below(&self, id: NodeId) -> Counts {
        self.below_info(id).counts
    }

    /// What the most recently busy subagent under `id` is doing.
    fn latest(&self, id: NodeId) -> Option<&Node> {
        self.below_info(id)
            .latest
            .and_then(|latest| self.tree.live.get(&latest))
    }

    /// Everything the closed row of a subtree says, computed once per change of the tree: each
    /// node is visited once however many rows ask, so a frame costs the rows it shows.
    fn below_info(&self, id: NodeId) -> Below {
        {
            let mut memo = self.tree.below_memo.borrow_mut();
            if memo.revision != self.tree.revision {
                memo.revision = self.tree.revision;
                memo.nodes.clear();
            }
            if let Some(known) = memo.nodes.get(&id) {
                return *known;
            }
        }
        let mut info = Below::default();
        if let Some(node) = self.tree.live.get(&id) {
            let mut latest_at = None;
            for child in &node.children {
                let Some(child_node) = self.tree.live.get(child) else {
                    continue;
                };
                if child_node.idle {
                    info.counts.idle += 1;
                } else {
                    info.counts.running += 1;
                }
                let deeper = self.below_info(*child);
                info.counts.add(deeper.counts);
                for candidate in [Some(*child), deeper.latest].into_iter().flatten() {
                    let Some(candidate_node) = self.tree.live.get(&candidate) else {
                        continue;
                    };
                    if !candidate_node.activity.is_empty()
                        && latest_at.is_none_or(|at| candidate_node.work_at > at)
                    {
                        latest_at = Some(candidate_node.work_at);
                        info.latest = Some(candidate);
                    }
                }
            }
            for summary in &node.nested {
                summary.add_counts(&mut info.counts);
            }
        }
        self.tree.below_memo.borrow_mut().nodes.insert(id, info);
        info
    }

    fn progress(&self, node: &Node) -> Progress {
        if node.idle {
            Progress::Idle
        } else {
            Progress::Running
        }
    }

    fn glyph(&self, chrome: &Chrome, node: &Node) -> Span<'static> {
        chrome.glyph(
            self.progress(node),
            Some(node.started_at),
            self.tree.animations(),
        )
    }

    /// `label  agent/model · state · elapsed · activity`, most important first so a narrow row
    /// loses the activity before the name.
    fn facts(
        &self,
        chrome: &Chrome,
        id: NodeId,
        node: &Node,
        with_elapsed: bool,
        with_activity: bool,
    ) -> Vec<Segment> {
        // Name, then state, model, elapsed, activity: a narrow row loses them from the end.
        let mut segments = vec![Segment::new(
            0,
            vec![Span::styled(node.label(), chrome.strong())],
        )];
        let who = who(chrome, node);
        if !who.is_empty() {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(who);
            segments.push(Segment::new(2, spans));
        }
        if node.idle {
            segments.push(Segment::new(1, vec![chrome.dot(), chrome.fact("idle")]));
        }
        let elapsed = self.now.saturating_duration_since(node.started_at);
        if with_elapsed && let Some(took) = format_duration(elapsed) {
            segments.push(Segment::new(3, vec![chrome.dot(), chrome.fact(took)]));
        }
        if with_activity && !node.idle {
            // Nothing reported yet is a state; what it reported is activity.
            let rank = if node.activity.trim().is_empty() {
                1
            } else {
                4
            };
            let mut spans = vec![chrome.dot()];
            spans.extend(self.doing(chrome, id, node));
            segments.push(Segment::new(rank, spans));
        }
        segments
    }

    /// What a subagent is doing, said honestly: one that has not reported anything itself but
    /// has helpers running is starting, and says how many are running under it.
    fn doing(&self, chrome: &Chrome, id: NodeId, node: &Node) -> Vec<Span<'static>> {
        if node.idle {
            return vec![chrome.fact("idle")];
        }
        if !node.activity.trim().is_empty() {
            return activity_spans(chrome, &node.activity);
        }
        let running = self.below(id).running;
        let text = match running {
            0 => node.task().map_or_else(
                || "starting…".to_owned(),
                |task| format!("starting · {task}"),
            ),
            1 => "starting · 1 child running".to_owned(),
            count => format!("starting · {count} children running"),
        };
        vec![Span::styled(text, chrome.faint())]
    }

    /// `⋮ 4 nested · ●2 ✓1 ✗1 · what is happening in it`.
    fn subtree_summary(&self, chrome: &Chrome, id: NodeId, width: usize) -> Vec<Span<'static>> {
        let counts = self.below(id);
        let mut segments = vec![Segment::new(
            0,
            vec![
                Span::styled("⋮ ", chrome.faint()),
                Span::styled(format!("{} nested", counts.total()), chrome.muted()),
            ],
        )];
        let mut by_state = vec![chrome.dot()];
        by_state.extend(counts.spans(chrome));
        segments.push(Segment::new(1, by_state));
        if let Some(latest) = self.latest(id) {
            let mut activity = vec![chrome.dot()];
            activity.extend(activity_spans(chrome, &latest.activity));
            segments.push(Segment::new(4, activity));
        }
        fit_segments(segments, width)
    }

    /// The block at `width`. It is laid out again only when the tree changed, the width or focus
    /// did, or the clock moved (a spinner frame while children animate, else a second).
    fn layout(&self, width: u16) -> Laid {
        let tick = self
            .now
            .saturating_duration_since(self.tree.created_at)
            .as_millis()
            / if self.tree.animations() {
                SPINNER_PERIOD.as_millis()
            } else {
                1000
            };
        let key = (
            self.tree.revision,
            width,
            self.screen_rows,
            self.focused,
            tick,
        );
        if let Some(cached) = self.tree.lane_cache.borrow().as_ref()
            && cached.key == key
        {
            return Laid {
                lines: cached.lines.clone(),
                hits: cached.hits.clone(),
            };
        }
        let laid = self.layout_uncached(width);
        *self.tree.lane_cache.borrow_mut() = Some(LaneCache {
            key,
            lines: laid.lines.clone(),
            hits: laid.hits.clone(),
        });
        laid
    }

    fn layout_uncached(&self, width: u16) -> Laid {
        let chrome = Chrome::live();
        let width = usize::from(width).max(16);
        let roots = self.roots();
        if roots.is_empty() {
            return Laid {
                lines: Vec::new(),
                hits: Vec::new(),
            };
        }
        let mut lines = vec![self.header(&chrome, width)];
        let mut hits = Vec::new();
        let budget = self.budget();
        let parallel = width >= PARALLEL_MIN_WIDTH && roots.len() >= 2 && self.focused.is_none();
        if parallel {
            self.lanes(&chrome, &roots, width, budget, &mut lines, &mut hits);
        } else {
            self.tree_rows(&chrome, &roots, width, budget, &mut lines, &mut hits);
        }
        Laid { lines, hits }
    }

    /// `◈ main · 3 running  ○1 ✓2`: every subagent of this burst of work, wherever it sits in
    /// the tree, the same numbers the status line shows.
    fn header(&self, chrome: &Chrome, width: usize) -> Line<'static> {
        let counts = self.tree.census();
        let mut segments = vec![Segment::new(
            0,
            vec![
                Span::styled("◈ ", chrome.violet()),
                Span::styled("main", chrome.strong()),
                chrome.dot(),
                Span::styled(format!("{} running", counts.running), chrome.muted()),
            ],
        )];
        if counts.idle + counts.done + counts.failed + counts.cancelled > 0 {
            let mut rest = vec![Span::raw("  ")];
            rest.extend(
                Counts {
                    running: 0,
                    ..counts
                }
                .spans(chrome),
            );
            segments.push(Segment::new(1, rest));
        }
        Line::from(fit_segments(segments, width))
    }

    /// The tree as rows: one per subagent, a compressed row under a subagent whose subtree is
    /// closed.
    fn tree_rows(
        &self,
        chrome: &Chrome,
        roots: &[NodeId],
        width: usize,
        budget: usize,
        lines: &mut Vec<Line<'static>>,
        hits: &mut Vec<(Rect, SubagentKey)>,
    ) {
        let open = self.open_path();
        let mut shown_roots = 0;
        for (index, root) in roots.iter().enumerate() {
            if lines.len() + 1 >= budget {
                break;
            }
            shown_roots += 1;
            let last = index + 1 == roots.len();
            self.push_node(chrome, *root, "", last, width, budget, &open, lines, hits);
        }
        if shown_roots < roots.len() {
            lines.push(Line::from(Span::styled(
                format!("⋯ +{} more", roots.len() - shown_roots),
                chrome.faint(),
            )));
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_node(
        &self,
        chrome: &Chrome,
        id: NodeId,
        prefix: &str,
        last: bool,
        width: usize,
        budget: usize,
        open: &BTreeSet<NodeId>,
        lines: &mut Vec<Line<'static>>,
        hits: &mut Vec<(Rect, SubagentKey)>,
    ) {
        let Some(node) = self.tree.live.get(&id) else {
            return;
        };
        let connector = if last { "└─ " } else { "├─ " };
        let rail = format!("{prefix}{connector}");
        let rail_width = rail.chars().count();
        let mut spans = vec![
            Span::styled(rail, chrome.faint()),
            self.glyph(chrome, node),
            Span::raw(" "),
        ];
        spans.extend(fit_segments(
            self.facts(chrome, id, node, true, true),
            width.saturating_sub(rail_width + 2),
        ));
        let row = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        hits.push((
            Rect::new(0, row, u16::try_from(width).unwrap_or(u16::MAX), 1),
            SubagentKey(id.0),
        ));
        lines.push(Line::from(spans));

        let child_prefix = format!("{prefix}{}", if last { "   " } else { "│  " });
        let has_below = !node.children.is_empty() || !node.nested.is_empty();
        if !has_below {
            return;
        }
        if open.contains(&id) {
            let children: Vec<NodeId> = node
                .children
                .iter()
                .copied()
                .filter(|child| self.tree.live.contains_key(child))
                .collect();
            for (index, child) in children.iter().enumerate() {
                if lines.len() + 1 >= budget {
                    lines.push(Line::from(Span::styled(
                        format!("{child_prefix}⋯ +{} more", children.len() - index),
                        chrome.faint(),
                    )));
                    return;
                }
                let child_last = index + 1 == children.len() && node.nested.is_empty();
                self.push_node(
                    chrome,
                    *child,
                    &child_prefix,
                    child_last,
                    width,
                    budget,
                    open,
                    lines,
                    hits,
                );
            }
            if !node.nested.is_empty() {
                let mut finished = Counts::default();
                for summary in &node.nested {
                    summary.add_counts(&mut finished);
                }
                let mut by_state = vec![Span::raw("  ")];
                by_state.extend(finished.spans(chrome));
                lines.push(Line::from(fit_segments(
                    vec![
                        Segment::new(
                            0,
                            vec![
                                Span::styled(format!("{child_prefix}└─ "), chrome.faint()),
                                Span::styled(
                                    format!("{} finished", finished.total()),
                                    chrome.muted(),
                                ),
                            ],
                        ),
                        Segment::new(1, by_state),
                    ],
                    width,
                )));
            }
        } else {
            let mut spans = vec![Span::styled(child_prefix, chrome.faint())];
            let room = width.saturating_sub(spans_width(&spans));
            spans.extend(self.subtree_summary(chrome, id, room));
            lines.push(Line::from(spans));
        }
    }

    /// Parallel children as cards side by side, three rows each: who, what, what is under it.
    fn lanes(
        &self,
        chrome: &Chrome,
        roots: &[NodeId],
        width: usize,
        budget: usize,
        lines: &mut Vec<Line<'static>>,
        hits: &mut Vec<(Rect, SubagentKey)>,
    ) {
        let columns = columns_for(width, LANE_MIN_WIDTH, LANE_GAP, MAX_LANE_COLUMNS);
        let card_width = (width - LANE_GAP * (columns - 1)) / columns;
        let mut shown = 0;
        for band in roots.chunks(columns) {
            let cards: Vec<Vec<Line<'static>>> = band
                .iter()
                .filter_map(|id| Some(self.card(chrome, *id, self.tree.live.get(id)?, card_width)))
                .collect();
            let height = cards.iter().map(Vec::len).max().unwrap_or(0);
            // A shorter card is filled out with its own surface, so a band is a clean block.
            let filler = if chrome.has_surfaces() {
                Line::from(" ".repeat(card_width)).style(chrome.theme.card)
            } else {
                Line::default()
            };
            let cards: Vec<Vec<Line<'static>>> = cards
                .into_iter()
                .map(|mut card| {
                    card.resize(height, filler.clone());
                    card
                })
                .collect();
            if lines.len() + height + 1 > budget && shown > 0 {
                break;
            }
            let top = u16::try_from(lines.len()).unwrap_or(u16::MAX);
            for (column, id) in band.iter().enumerate() {
                hits.push((
                    Rect::new(
                        u16::try_from(column * (card_width + LANE_GAP)).unwrap_or(u16::MAX),
                        top,
                        u16::try_from(card_width).unwrap_or(u16::MAX),
                        u16::try_from(height).unwrap_or(u16::MAX),
                    ),
                    SubagentKey(id.0),
                ));
            }
            shown += band.len();
            lines.extend(side_by_side(cards, card_width, LANE_GAP));
        }
        if shown < roots.len() {
            lines.push(Line::from(Span::styled(
                format!("⋯ +{} more", roots.len() - shown),
                chrome.faint(),
            )));
        }
    }

    /// One lane: an accent bar on the card surface, then who, what it is doing now, and what is
    /// under it.
    fn card(&self, chrome: &Chrome, id: NodeId, node: &Node, width: usize) -> Vec<Line<'static>> {
        let surface = chrome.theme.card;
        let bar = Span::styled("▌ ", chrome.tone(self.progress(node)));
        let inner = width.saturating_sub(2);
        let mut rows: Vec<Vec<Span<'static>>> = Vec::new();

        let elapsed = self.now.saturating_duration_since(node.started_at);
        let took = format_duration(elapsed).unwrap_or_default();
        let mut segments = vec![Segment::new(
            0,
            vec![self.glyph(chrome, node), Span::raw(" ")],
        )];
        segments.extend(self.facts(chrome, id, node, false, false));
        let room = inner.saturating_sub(took.chars().count() + 1);
        let mut head = fit_segments(segments, room);
        let used = spans_width(&head);
        head.push(Span::raw(
            " ".repeat(inner.saturating_sub(used + took.chars().count())),
        ));
        head.push(Span::styled(took, chrome.muted()));
        rows.push(head);

        rows.push(truncate_spans(self.doing(chrome, id, node), inner));
        if !node.children.is_empty() || !node.nested.is_empty() {
            rows.push(self.subtree_summary(chrome, id, inner));
        }

        rows.into_iter()
            .map(|mut spans| {
                spans.insert(0, bar.clone());
                let used = spans_width(&spans);
                if used < width {
                    spans.push(Span::raw(" ".repeat(width - used)));
                }
                if chrome.has_surfaces() {
                    for span in &mut spans {
                        span.style = surface.patch(span.style);
                    }
                }
                Line::from(spans).style(surface)
            })
            .collect()
    }
}

/// `agent/model`, the model without its provider prefix; the child's type when neither is known.
fn who(chrome: &Chrome, node: &Node) -> Vec<Span<'static>> {
    let identity = chrome.identity(node.agent.as_deref(), node.model.as_deref());
    if !identity.is_empty() {
        return identity;
    }
    node.kind()
        .map(|kind| vec![Span::styled(kind, chrome.muted())])
        .unwrap_or_default()
}

/// What a subagent is doing: a shell command is highlighted like any other, the rest is muted.
fn activity_spans(chrome: &Chrome, activity: &str) -> Vec<Span<'static>> {
    let activity = activity.trim();
    if activity.is_empty() {
        return vec![Span::styled("starting…", chrome.faint())];
    }
    let activity = truncate_tail(activity, 160);
    if let Some(command) = activity.strip_prefix("$ ") {
        let mut spans = vec![Span::styled("$ ", chrome.mint())];
        spans.extend(highlight_bash_spans(command));
        return spans;
    }
    if let Some((verb, command)) = activity
        .strip_prefix("Ran ")
        .map(|command| ("Ran ", command))
        .or_else(|| {
            activity
                .strip_prefix("Running ")
                .map(|command| ("Running ", command))
        })
    {
        let mut spans = vec![Span::styled(verb, chrome.muted())];
        spans.extend(highlight_bash_spans(command));
        return spans;
    }
    vec![Span::styled(activity, chrome.muted())]
}

impl Renderable for InChatSubagents<'_> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut hits = self.hits.borrow_mut();
        hits.clear();
        if area.is_empty() {
            return;
        }
        let laid = self.layout(area.width);
        for (rect, key) in laid.hits {
            let placed = Rect::new(area.x + rect.x, area.y + rect.y, rect.width, rect.height);
            if placed.intersection(area) == placed {
                hits.push((placed, key));
            }
        }
        Paragraph::new(laid.lines).render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.layout(width).lines.len()).unwrap_or(u16::MAX)
    }
}
