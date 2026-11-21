//! The stage: what the primary screen shows of the conversation, and what of it is the
//! terminal's for good.
//!
//! Nori never uses the alternate screen. The conversation is the terminal's own text: it scrolls
//! into the scrollback, re-wraps on a resize and is selected like any program's output. Nori
//! *prints* a row of it, hands it to the terminal for good, as soon as the row can no longer
//! change: its cell and every cell before it have settled (nothing running or animating). A
//! printed row is never drawn again. Below the printed rows is the *region*, the only part of the
//! screen Nori redraws: the rows of the conversation that may still change (the *live tail*, from
//! the first cell that has not settled) and, under them, the bottom block (what the running turn
//! shows, the composer and its status line). At rest the region is the bottom block alone.
//!
//! Rows leave the screen from the top, only when what is on it no longer fits: line feeds on the
//! last row ([`write::scroll_rows_off`]) push the screen up, printed rows first, as they are. A
//! row of the live tail that leaves is written at the top just before its line feed when the top
//! does not show it. Either way it reaches the scrollback once, in order, and every row that stays
//! is moved by the terminal, never drawn again: what the person reads or selects keeps its place.
//!
//! A *view* (a popup, a picker, an approval) and a decoration of the conversation (a focused
//! prompt, an expanded card) *reclaim* the printed rows still on screen: the region grows over
//! them (the terminal's buffer of the last frame is told exactly what they hold, so the ones that
//! do not change are not written), the view clips them from the top, and they are printed again
//! once it closes. Rows a resize re-wrapped are the terminal's alone and are never reclaimed.
//!
//! Once the conversation has pushed the bottom block to the screen's last row it stays there; when
//! it shrinks, the rows it frees stay empty above it, inside the region, until the conversation
//! grows into them.

mod focus;
mod rows;
#[cfg(test)]
pub(crate) mod sim;
mod tail;
pub(crate) mod write;

use std::io;
use std::io::Write;
use std::sync::Arc;

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::text::Line;

pub(crate) use self::focus::FocusView;
pub(crate) use self::focus::FocusedColumn;
pub(crate) use self::focus::InlineEdit;
pub(crate) use self::focus::render_focused;
use self::rows::RowCache;
use self::tail::Block;
use self::tail::Boundary;
use self::tail::advance;
use crate::custom_terminal::Terminal;
use crate::history_cell::HistoryCell;

/// What the stage draws this frame: the conversation and how it is decorated.
pub(crate) struct StageView<'a> {
    pub(crate) cells: &'a [Arc<dyn HistoryCell>],
    /// The prompt a rewind is focused on, if any.
    pub(crate) focus: Option<&'a FocusView>,
    /// A turn is running: the last cell may still grow (a read joining the group of reads
    /// before it, a tool's late result), so it is not printed yet.
    pub(crate) open: bool,
}

impl<'a> StageView<'a> {
    pub(crate) fn new(cells: &'a [Arc<dyn HistoryCell>], focus: Option<&'a FocusView>) -> Self {
        Self {
            cells,
            focus,
            open: false,
        }
    }

    /// The same view while a turn is running.
    pub(crate) fn open(self, open: bool) -> Self {
        Self { open, ..self }
    }
}

impl StageView<'static> {
    /// A view with nothing in it, for screens that are not the conversation.
    pub(crate) fn empty() -> Self {
        Self {
            cells: &[],
            focus: None,
            open: false,
        }
    }
}

/// Where an expandable card is on screen, for clicks: the cell's place in the conversation.
#[derive(Debug)]
struct Hit {
    rows: std::ops::Range<u16>,
    cell: usize,
}

/// The rows of the conversation printed and still on screen right above the region: the
/// terminal's, never drawn again unless a view reclaims them. They are kept as they were written,
/// for the width they were written at, so that what a resize did to them can be worked out.
#[derive(Default)]
struct Printed {
    rows: Vec<Line<'static>>,
    width: u16,
}

#[derive(Default)]
pub(crate) struct Stage {
    /// The first row of the conversation that is not in the scrollback.
    boundary: Boundary,
    /// The rows from the boundary on that are printed.
    printed: Printed,
    /// The next frame reclaims the printed rows: cells they show are gone.
    reclaim_next: bool,
    cache: RowCache,
    /// The rows the last frame drew from the top of the region, when they were the whole live
    /// tail. A row that leaves need not be written first when it is one of these.
    shown: Option<Vec<Line<'static>>>,
    /// The blocks of the tail as the last frame left them, after its commit.
    blocks: Vec<Block>,
    /// The rows the last frame drew, whether or not they were the whole tail.
    displayed: Vec<Line<'static>>,
    displayed_whole: bool,
    clear_requested: bool,
    hits: Vec<Hit>,
    /// How many rows each committed cell put in the scrollback (its blank row included), by its
    /// place in the conversation. Replacing cells that are partly or wholly in the scrollback
    /// (the streamed pieces of an answer merged into one cell) must know how many of the new
    /// cells' rows are there already, long after the row cache has forgotten them.
    committed_rows: Vec<usize>,
    /// The conversation pushed the bottom block to the screen's last row, where it stays, a resize
    /// included: rows it frees, or a taller screen adds, are left empty right above it.
    anchored: bool,
}

/// Everything one frame does, decided before anything is written, for one size of the screen:
/// a resize while it is written is the next frame's to find.
pub(crate) struct Plan {
    /// The screen the frame is for.
    screen: Size,
    /// Rows the screen scrolls up.
    scroll: u16,
    /// Rows of the tail (printed ones first) that end in the scrollback.
    commit: usize,
    /// The screen row of the first row of the tail that is drawn or printed.
    new_top: u16,
    /// Every row of the tail, from the boundary (or, decorated, from before it).
    rows: Vec<Line<'static>>,
    /// The first of them that is on screen, and how many are.
    first: usize,
    count: usize,
    /// How many of those are printed and stay the terminal's this frame: the region starts after
    /// them.
    owned: usize,
    /// The printed rows were taken back: the region starts at the first conversation row on
    /// screen.
    reclaimed: bool,
    /// And the screen still shows them as `rows` has them (no cell under them went).
    seeded: bool,
    /// How many rows were printed before this frame, and so how the screen held them.
    printed_before: usize,
    /// Where the bottom block is drawn.
    block: Rect,
    /// The blocks of the tail, and the expandable card each of its rows belongs to.
    blocks: Vec<Block>,
    cards: Vec<(std::ops::Range<usize>, usize)>,
    /// Whether the tail is decorated, and so clipped rather than scrolled.
    decorated: bool,
    /// A view covers the top of the tail.
    covered: bool,
    /// How many of the first rows belong to settled cells.
    settled: usize,
    /// The region covers the whole screen below its top: only the bottom block is drawn.
    full: bool,
    /// The whole tail is on screen, from the top of the region.
    whole: bool,
    /// Where the cursor of a prompt edited in place is: `(row, column)` of the tail.
    focus_cursor: Option<(usize, u16)>,
}

impl Plan {
    /// The frame the bottom block draws in, in screen coordinates.
    pub(crate) fn block(&self) -> Rect {
        self.block
    }

    /// Where the cursor goes when a prompt is edited in place in the conversation, in the
    /// coordinates of a buffer whose first row is the region's: on its row of the tail, when
    /// that row is on screen.
    pub(crate) fn focus_cursor(&self, region: Rect) -> Option<(u16, u16)> {
        let (row, column) = self.focus_cursor?;
        let from = (self.first + self.owned).min(self.first + self.count);
        if row < from || row >= self.first + self.count {
            return None;
        }
        let y = region.y + u16::try_from(row - from).ok()?;
        (y < region.bottom()).then_some((region.x + column, y))
    }

    /// The first screen row of the region: the rows above it are not drawn.
    pub(crate) fn region_top(&self) -> u16 {
        self.new_top
            .saturating_add(u16::try_from(self.owned).unwrap_or(u16::MAX))
            .min(self.block.y)
    }
}

impl Stage {
    /// Give the terminal the conversation from `cell` on: the cells before it are never drawn
    /// or committed. A resumed conversation brings its last screens into the scrollback, not its
    /// whole past, which would cost a render of every cell and outgrow the terminal's history.
    pub(crate) fn start_at(&mut self, cell: usize) {
        self.boundary = Boundary {
            cell,
            skip: 0,
            of: 0,
            fresh: true,
        };
        self.printed = Printed::default();
        self.cache.forget_before(cell);
        self.blocks.clear();
        self.shown = None;
        self.committed_rows.resize(cell, 0);
    }

    /// Push everything on screen into the scrollback at the next frame, and start a clean region.
    pub(crate) fn request_clear(&mut self) {
        self.clear_requested = true;
    }

    /// The screen may no longer show what was drawn (a resize, a suspend): rows that leave are
    /// written at the top before they do until a frame has drawn the tail again.
    pub(crate) fn forget_screen(&mut self) {
        self.shown = None;
    }

    /// The screen grew taller and the terminal pulled no rows back from the scrollback: the rows
    /// it added are empty, at the bottom. The bottom block comes up under the conversation again
    /// rather than staying on the last row over a gap, which a later, shorter screen would push
    /// the conversation off the top to keep.
    pub(crate) fn unanchor_after_bare_grow(&mut self) {
        self.anchored = false;
    }

    /// The printed rows are the terminal's alone from now on, like the shell's own output, and are
    /// never reclaimed (a resize moved them and nothing says where).
    pub(crate) fn forget_printed(&mut self) {
        let rows = std::mem::take(&mut self.printed).rows.len();
        if rows > 0 {
            let blocks = std::mem::take(&mut self.blocks);
            let (boundary, left) = advance(self.boundary, &blocks, rows);
            self.record_committed(&blocks, boundary.cell);
            self.boundary = boundary;
            self.blocks = left;
        }
    }

    /// After a resize, with the region now starting `above` rows down the screen: how the
    /// terminal re-wrapped the printed rows to `width` says which of them are still on screen,
    /// counted up from the region. Every cell still wholly there is taken back, to be laid out
    /// again at the new width by the next frame (and printed again); the rest (cut by the top of
    /// the screen, or pushed into the scrollback) are the terminal's alone. Says how many screen
    /// rows the cells taken back cover, right above the region, which the region now starts
    /// that much higher to include.
    pub(crate) fn reown_after_resize(
        &mut self,
        above: u16,
        width: u16,
        change: crate::tui::reflow::WidthChange,
    ) -> u16 {
        let printed = std::mem::take(&mut self.printed);
        if printed.rows.is_empty() {
            return 0;
        }
        let heights: Vec<usize> = printed
            .rows
            .iter()
            .map(|row| change.rows(rewrapped_height(row, printed.width, width)))
            .collect();
        // The printed rows by cell, from the boundary: where each cell's rows start, how many
        // there are, and whether they are all of its rows.
        let mut cells = Vec::new();
        let mut offset = 0;
        for block in &self.blocks {
            if offset >= heights.len() {
                break;
            }
            let rows = block.rows.min(heights.len() - offset);
            cells.push((
                offset,
                rows,
                block.rows == block.total && rows == block.rows,
            ));
            offset += rows;
        }
        let (mut taken_rows, mut taken_height) = (0, 0);
        for (start, rows, whole) in cells.into_iter().rev() {
            let height: usize = heights[start..start + rows].iter().sum();
            if !whole || taken_height + height > usize::from(above) {
                break;
            }
            taken_rows += rows;
            taken_height += height;
        }
        let before = heights.len() - taken_rows;
        if before > 0 {
            let blocks = std::mem::take(&mut self.blocks);
            let (boundary, left) = advance(self.boundary, &blocks, before);
            self.record_committed(&blocks, boundary.cell);
            self.boundary = boundary;
            self.blocks = left;
        }
        self.shown = None;
        u16::try_from(taken_height).unwrap_or(0)
    }

    /// The expandable card nearest the composer among the cells of `view` on screen, drawn or
    /// printed.
    pub(crate) fn last_card(&self, view: &StageView<'_>) -> Option<usize> {
        view.cells
            .iter()
            .enumerate()
            .skip(self.boundary.cell)
            .rev()
            .find(|(_, cell)| cell.expansion().is_some())
            .map(|(index, _)| index)
    }

    /// The expandable card drawn at `row` of the screen.
    pub(crate) fn card_at(&self, row: u16) -> Option<usize> {
        self.hits
            .iter()
            .find(|hit| hit.rows.contains(&row))
            .map(|hit| hit.cell)
    }

    /// Whether the terminal pushed `rows` rows of the region's tail into the scrollback behind our
    /// back (a resize shrank the screen), so they must not be drawn again. The printed rows are
    /// forgotten first ([`Self::forget_printed`]).
    pub(crate) fn note_lost_rows(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        let blocks = std::mem::take(&mut self.blocks);
        let (boundary, left) = advance(self.boundary, &blocks, rows);
        self.record_committed(&blocks, boundary.cell);
        self.boundary = boundary;
        self.blocks = left;
        self.shown = None;
    }

    /// Note the rows of every cell before `to` that `blocks` shows were committed whole.
    fn record_committed(&mut self, blocks: &[Block], to: usize) {
        if self.committed_rows.len() < to {
            self.committed_rows.resize(to, 0);
        }
        for block in blocks.iter().filter(|block| block.cell < to) {
            self.committed_rows[block.cell] = block.total;
        }
    }

    /// The conversation lost every cell from `len` on. Printed rows of them still on screen are
    /// taken back, so they go.
    pub(crate) fn truncated(&mut self, len: usize) {
        self.reclaim_next |= !self.printed.rows.is_empty();
        if self.boundary.cell > len {
            self.boundary = Boundary {
                cell: len,
                ..Boundary::default()
            };
        }
        self.committed_rows.truncate(len);
        self.cache.forget_from(len);
        self.shown = None;
    }

    /// The cells of `range` were replaced by `new_len` cells carrying the same content. Rows of
    /// them that are in the scrollback already stay committed.
    pub(crate) fn replaced(&mut self, range: std::ops::Range<usize>, new_len: usize) {
        let old_len = range.len();
        let committed_in = |rows: &[usize], to: usize| -> usize {
            rows.get(range.start..to.min(rows.len()))
                .map_or(0, |rows| rows.iter().sum())
        };
        if self.boundary.cell >= range.end {
            // The whole range is in the scrollback: the new cells stand for its rows.
            let rows = committed_in(&self.committed_rows, range.end);
            self.boundary.cell = self.boundary.cell - old_len + new_len;
            if self.committed_rows.len() >= range.end {
                let mut replacement = vec![0; new_len];
                if let Some(first) = replacement.first_mut() {
                    *first = rows;
                }
                self.committed_rows.splice(range.clone(), replacement);
            }
        } else if self.boundary.cell >= range.start {
            // Part of the range is in the scrollback: count its rows, as they were committed.
            let committed =
                committed_in(&self.committed_rows, self.boundary.cell) + self.boundary.skip;
            self.boundary = Boundary {
                cell: range.start,
                skip: committed,
                of: 0,
                fresh: false,
            };
            self.committed_rows.truncate(range.start);
        }
        self.cache.forget_from(range.start);
        self.shown = None;
    }

    /// Everything drawn so far is history now (the shell wrote below it): it will not be drawn
    /// or committed again.
    pub(crate) fn release_tail(&mut self) {
        let rows = self.blocks.iter().map(|block| block.rows).sum();
        self.printed = Printed::default();
        self.note_lost_rows(rows);
    }

    /// The rows the last frame drew in the region's tail, when they were the tail itself (not a
    /// view of it, such as a focused prompt over rows that may be in the scrollback already).
    pub(crate) fn displayed(&self) -> &[Line<'static>] {
        if self.displayed_whole {
            &self.displayed
        } else {
            &[]
        }
    }

    /// Decide this frame. `top` is the first row of the region, `height` the rows of the bottom
    /// block of which `popup_rows` are a view that covers the tail instead of pushing it up.
    pub(crate) fn plan(
        &mut self,
        view: &StageView<'_>,
        screen: Size,
        top: u16,
        height: u16,
        popup_rows: u16,
    ) -> Plan {
        let full = height >= screen.height;
        let height = height.clamp(1, screen.height.max(1));
        if full {
            // A full-screen view: a picker, the onboarding, a child's conversation. The printed
            // rows stay the terminal's.
            self.shown = None;
            self.hits.clear();
            let available = screen.height.saturating_sub(top);
            return Plan {
                screen,
                scroll: 0,
                commit: 0,
                new_top: top,
                rows: Vec::new(),
                first: 0,
                count: 0,
                owned: 0,
                reclaimed: false,
                seeded: false,
                printed_before: self.printed.rows.len(),
                block: Rect::new(0, top, screen.width, available),
                blocks: Vec::new(),
                cards: Vec::new(),
                decorated: false,
                covered: false,
                settled: 0,
                full: true,
                whole: false,
                focus_cursor: None,
            };
        }
        let tail = self.tail(view, screen.width);
        let decorated = view.focus.is_some() || tail.expanded;
        let covered = popup_rows > 0;
        // The printed rows sit right above the region: the conversation on screen starts there.
        let printed_before = self.printed.rows.len().min(usize::from(top));
        let conversation_top = top - u16::try_from(printed_before).unwrap_or(0);
        // A view or a decoration takes the printed rows back to draw over them; they are printed
        // again once it closes. Rows whose cells went are taken back to be drawn again.
        let changed = std::mem::take(&mut self.reclaim_next);
        let reclaimed = decorated || covered || changed;
        // The block decides what the conversation hands to the scrollback, except for a view on
        // top of it, which clips the tail from the top instead: the clipped rows are still the
        // stage's, and come back the moment it closes. Nothing is ever drawn under it.
        let base = height.saturating_sub(popup_rows).max(1);
        let available = screen.height.saturating_sub(conversation_top);
        let rows_len = tail.rows.len();
        let wanted = rows_len.saturating_add(usize::from(base));
        let overflow = wanted.saturating_sub(usize::from(available));
        // The conversation and the block fill the screen down to its last row.
        let fills = wanted >= usize::from(available);

        let (mut scroll, mut commit, mut new_top, mut first) =
            (0u16, 0usize, conversation_top, 0usize);
        let cleared = std::mem::take(&mut self.clear_requested);
        if cleared {
            // Everything above the bottom block goes to the scrollback and the block moves up.
            let above = usize::from(conversation_top) + rows_len;
            scroll = u16::try_from(above).unwrap_or(u16::MAX).min(screen.height);
            commit = rows_len;
            new_top = 0;
            first = rows_len;
        } else if overflow > 0 && !decorated {
            scroll = u16::try_from(overflow).unwrap_or(u16::MAX);
            commit = overflow.saturating_sub(usize::from(conversation_top));
            new_top = conversation_top.saturating_sub(scroll);
            first = commit;
        }
        // A block taller than the rows below the conversation pushes the shell's own rows above
        // it into the scrollback, as any program printing that much would.
        let short = height.saturating_sub(screen.height.saturating_sub(new_top));
        if short > 0 {
            let lift = short.min(new_top);
            scroll = scroll.saturating_add(lift);
            new_top -= lift;
        }
        let available = screen.height.saturating_sub(new_top);
        // The tail gets the rows above the whole block. What does not fit is clipped from the
        // top, keeping a focused prompt in view.
        let room = usize::from(available.saturating_sub(height));
        let visible = rows_len.saturating_sub(first);
        if visible > room {
            first += visible - room;
            if let Some(selected) = tail.focus_rows.clone() {
                first = first.min(selected.start);
            }
        }
        let count = rows_len.saturating_sub(first).min(room);
        // Printed rows still on screen after the commit stay the terminal's: not drawn.
        let owned = if reclaimed {
            0
        } else {
            printed_before.saturating_sub(commit).min(count)
        };
        let count_rows = u16::try_from(count).unwrap_or(u16::MAX);
        let bottom = screen.height.saturating_sub(height);
        // A block the conversation pushed to the last row stays there when it shrinks: the rows
        // it frees stay empty above it, rather than the prompt floating up. A view that merely
        // reached down there does not hold it.
        let block_y = if self.anchored && !cleared {
            bottom
        } else {
            new_top.saturating_add(count_rows).min(bottom)
        };
        self.anchored = !cleared && (fills || self.anchored);
        Plan {
            screen,
            scroll,
            commit,
            new_top,
            first,
            count,
            owned,
            reclaimed,
            seeded: !changed && tail.from_boundary,
            printed_before,
            block: Rect::new(0, block_y, screen.width, height),
            blocks: tail.blocks,
            cards: tail.cards,
            decorated,
            covered,
            settled: if tail.from_boundary { tail.settled } else { 0 },
            full: false,
            whole: !decorated && first == commit,
            focus_cursor: tail.focus_cursor,
            rows: tail.rows,
        }
    }

    /// Carry out the scrolling of `plan`, and leave the terminal's region where the plan puts it.
    pub(crate) fn commit<B>(&mut self, terminal: &mut Terminal<B>, plan: &Plan) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        let screen = plan.screen;
        let top = terminal.viewport_area.y;
        let region_top = plan.region_top().min(screen.height.saturating_sub(1));
        let area = Rect::new(0, region_top, screen.width, screen.height - region_top);
        if plan.full {
            if terminal.viewport_area != area {
                terminal.set_viewport_area(area);
            }
            // The blocks still say which cells the printed rows above the view belong to: a
            // resize under the view takes them back, or hands them to the scrollback, by them.
            return Ok(());
        }
        let printed = plan.printed_before;
        let conversation_top = top.saturating_sub(u16::try_from(printed).unwrap_or(u16::MAX));
        if plan.scroll > 0 || plan.commit > 0 {
            // The rows above the conversation leave first, then the tail's, the printed ones as
            // they are and the others from the top of the screen. A row of the live tail the last
            // frame drew there is there already.
            let lift = plan.scroll.min(conversation_top);
            let leaving = &plan.rows[..plan.commit.min(plan.rows.len())];
            let shown = self.shown.as_deref().unwrap_or_default();
            write::scroll_rows_off(
                terminal.backend_mut(),
                screen.height,
                screen.width,
                lift,
                leaving,
                |index| {
                    index < printed
                        || index
                            .checked_sub(printed)
                            .and_then(|drawn| shown.get(drawn))
                            == leaving.get(index)
                },
            )?;
        }
        // The terminal's buffer of the last frame keeps what is still on screen. Rows the region
        // grows over (printed rows a view reclaims) are told to it as they are.
        let shift = i32::from(region_top) + i32::from(plan.scroll) - i32::from(top);
        if shift >= 0 {
            terminal.scroll_viewport(area, u16::try_from(shift).unwrap_or(u16::MAX));
        } else {
            let from = i32::from(conversation_top) - i32::from(plan.scroll);
            let seeds: Vec<Option<&Line<'static>>> = (0..-shift)
                .map(|row| {
                    let index = usize::try_from(i32::from(region_top) + row - from).ok()?;
                    // What the terminal shows there is the row as it was printed, not as this
                    // frame draws it: a focused prompt and what follows it look different now.
                    (plan.seeded && index < printed)
                        .then(|| self.printed.rows.get(index))
                        .flatten()
                })
                .collect();
            terminal.reclaim_viewport(area, &seeds);
        }
        if plan.commit > 0 || plan.scroll > 0 {
            let (boundary, blocks) = advance(self.boundary, &plan.blocks, plan.commit);
            let whole = plan.commit == plan.rows.len() && plan.scroll > 0;
            self.record_committed(
                &plan.blocks,
                if whole {
                    plan.blocks
                        .last()
                        .map_or(boundary.cell, |block| block.cell + 1)
                } else {
                    boundary.cell
                },
            );
            self.boundary = if whole {
                Boundary {
                    cell: plan
                        .blocks
                        .last()
                        .map_or(boundary.cell, |block| block.cell + 1),
                    skip: 0,
                    of: 0,
                    fresh: true,
                }
            } else {
                boundary
            };
            self.blocks = blocks;
        } else if !plan.decorated {
            self.blocks = plan.blocks.clone();
        }
        if plan.reclaimed {
            self.printed = Printed::default();
        } else {
            // The printed rows that scrolled off are in the scrollback now.
            let gone = plan.commit.min(self.printed.rows.len());
            self.printed.rows.drain(..gone);
        }
        Ok(())
    }

    /// Draw the live tail of `plan` into `buffer`, whose first row is the region's.
    pub(crate) fn draw(&mut self, plan: &Plan, buffer: &mut Buffer) {
        self.hits.clear();
        if plan.full {
            self.displayed.clear();
            return;
        }
        let area = buffer.area;
        let from = (plan.first + plan.owned).min(plan.first + plan.count);
        let to = plan.first + plan.count;
        for (offset, row) in plan.rows[from..to].iter().enumerate() {
            let Ok(offset) = u16::try_from(offset) else {
                break;
            };
            let y = area.y + offset;
            if y >= area.bottom() {
                break;
            }
            let rect = Rect::new(area.x, y, area.width, 1);
            buffer.set_style(rect, row.style);
            buffer.set_line(area.x, y, row, area.width);
        }
        for (range, cell) in &plan.cards {
            let start = range.start.max(from);
            let end = range.end.min(to);
            if start >= end {
                continue;
            }
            let row = |index: usize| area.y + u16::try_from(index - from).unwrap_or(0);
            self.hits.push(Hit {
                rows: row(start)..row(end),
                cell: *cell,
            });
        }
        self.displayed = plan.rows[from..to].to_vec();
        self.displayed_whole = plan.whole;
        self.shown = plan.whole.then(|| plan.rows[from..to].to_vec());
    }

    /// After a frame is written: the rows of settled cells it drew at the top of the region are
    /// printed, the terminal's for good. Says how many, so the region starts that much lower.
    pub(crate) fn settle(&mut self, plan: &Plan) -> usize {
        if plan.full || plan.decorated || plan.covered {
            return 0;
        }
        let from = plan.first + plan.owned;
        let to = plan.first + plan.count;
        let rows = plan.settled.min(to).saturating_sub(from);
        if rows == 0 {
            return 0;
        }
        self.printed.width = plan.screen.width;
        self.printed
            .rows
            .extend_from_slice(&plan.rows[from..from + rows]);
        for drawn in [&mut self.displayed].into_iter().chain(self.shown.as_mut()) {
            drawn.drain(..rows.min(drawn.len()));
        }
        rows
    }
}

/// How many rows `row`, written as a row `written` columns wide, takes once a terminal re-wraps
/// it to `width` columns: through its last glyph that is not a blank in the default background,
/// or to its end when the row itself has a background.
fn rewrapped_height(row: &Line<'_>, written: u16, width: u16) -> usize {
    use ratatui::style::Color;
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let extent = if row.style.bg.is_some_and(|bg| bg != Color::Reset) {
        usize::from(written)
    } else {
        let mut column = 0;
        let mut extent = 0;
        for span in &row.spans {
            let style = span.style.patch(row.style);
            let marked =
                style.bg.is_some_and(|bg| bg != Color::Reset) || !style.add_modifier.is_empty();
            for grapheme in span.content.graphemes(true) {
                let glyph = grapheme.width().max(1);
                if marked || !grapheme.trim().is_empty() {
                    extent = column + glyph;
                }
                column += glyph;
            }
        }
        extent
    };
    extent.max(1).div_ceil(usize::from(width.max(1)))
}
