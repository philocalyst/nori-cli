//! The stage: what the primary screen shows of the conversation, and what leaves it for the
//! scrollback.
//!
//! Nori never uses the alternate screen. The live region owns every row from where the program
//! started down to the bottom of the terminal; above it is the shell's own output and the
//! conversation that already scrolled off. Inside the region sit the cells of the conversation
//! that are not in the scrollback yet (the *tail*, drawn like any other widget, so a running tool,
//! an expanded card or a focused prompt can change in place) and, below them, the bottom block:
//! live tail, composer and status line.
//!
//! Rows leave the tail from the top, and only when the tail and the *resting* part of the bottom
//! block (composer and status line) no longer fit: the screen scrolls, the rows that pass the top
//! edge are in the scrollback from then on, and the *boundary* (the first row that is not) moves
//! past them. A row is in the scrollback exactly once, because nothing draws it again once the
//! boundary has passed it. Everything else in the bottom block (a popup, a panel, a running tool,
//! the lanes of running subagents) is transient and clips the tail at the top instead, as does a
//! decorated tail (a focused prompt, an expanded card): what the user or a live view put on screen
//! for a while never pushes rows away for good, and nothing is ever drawn under it.
//!
//! The two ways to commit rows are [`write::scroll_screen_up`], when the rows leaving are the
//! ones on screen, and [`write::type_rows`], when they are not (a long history appearing at once,
//! or rows that changed since they were drawn). Either way the rows pass through the screen in
//! order.

mod focus;
mod rows;
#[cfg(test)]
pub(crate) mod sim;
pub(crate) mod write;

use std::io;
use std::io::Write;
use std::sync::Arc;

use ratatui::backend::Backend;
use ratatui::buffer::Buffer;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::text::Line;

pub(crate) use self::focus::EditView;
pub(crate) use self::focus::FocusView;
pub(crate) use self::focus::FocusedColumn;
pub(crate) use self::focus::render_focused;
use self::rows::RowCache;
use crate::custom_terminal::Terminal;
use crate::history_cell::HistoryCell;

/// What the stage draws this frame: the conversation and how it is decorated.
pub(crate) struct StageView<'a> {
    pub(crate) cells: &'a [Arc<dyn HistoryCell>],
    /// The prompt a rewind is focused on, if any.
    pub(crate) focus: Option<&'a FocusView>,
}

impl<'a> StageView<'a> {
    pub(crate) fn new(cells: &'a [Arc<dyn HistoryCell>], focus: Option<&'a FocusView>) -> Self {
        Self { cells, focus }
    }
}

impl StageView<'static> {
    /// A view with nothing in it, for screens that are not the conversation.
    pub(crate) fn empty() -> Self {
        Self {
            cells: &[],
            focus: None,
        }
    }
}

/// The first row of the conversation that is not in the scrollback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Boundary {
    /// The cell it is in.
    cell: usize,
    /// How many rows of that cell (counting the blank row before it) are already committed.
    skip: usize,
    /// How many rows that cell had when `skip` was counted, so a width change can scale it.
    of: usize,
    /// The screen was just cleared: no blank row separates the next cell from what is above.
    fresh: bool,
}

/// The rows one cell contributes to the tail.
#[derive(Clone, Copy, Debug)]
struct Block {
    cell: usize,
    /// Rows left in the tail, after the ones already committed.
    rows: usize,
    /// Rows the block has in all.
    total: usize,
}

/// Where an expandable card is on screen, for clicks: the cell's place in the conversation.
#[derive(Debug)]
struct Hit {
    rows: std::ops::Range<u16>,
    cell: usize,
}

#[derive(Default)]
pub(crate) struct Stage {
    boundary: Boundary,
    cache: RowCache,
    /// The rows the last frame drew from the top of the region, when they were the whole tail
    /// starting at the boundary. Scrolling may only stand in for typing rows that are these.
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
}

/// How the rows that leave the region reach the scrollback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Commit {
    /// Nothing of the conversation leaves (a shell row may).
    None,
    /// The rows are on screen: scroll them off.
    Scroll,
    /// The rows are not on screen: type them, scrolling as the screen fills.
    Type,
}

/// Everything one frame does, decided before anything is written.
pub(crate) struct Plan {
    commit_mode: Commit,
    /// Rows the screen scrolls up.
    scroll: u16,
    /// Rows of the conversation that end in the scrollback.
    commit: usize,
    /// First screen row of the region afterwards.
    new_top: u16,
    /// Every row of the tail.
    rows: Vec<Line<'static>>,
    /// The first of them that is drawn, and how many.
    first: usize,
    count: usize,
    /// Where the bottom block is drawn.
    block: Rect,
    /// The blocks of the tail, and the expandable card each of its rows belongs to.
    blocks: Vec<Block>,
    cards: Vec<(std::ops::Range<usize>, usize)>,
    /// Whether the tail is decorated, and so clipped rather than scrolled.
    decorated: bool,
    /// The caret of a prompt being edited in place, as a row and column of `rows`.
    caret: Option<(usize, u16)>,
    /// The region covers the whole screen below its top: only the bottom block is drawn.
    full: bool,
    /// The whole tail is on screen, from the top of the region.
    whole: bool,
}

impl Plan {
    /// The frame the bottom block draws in, in screen coordinates.
    pub(crate) fn block(&self) -> Rect {
        self.block
    }

    /// Where the caret of an in-place edit is on screen.
    pub(crate) fn caret(&self) -> Option<Position> {
        let (row, column) = self.caret?;
        let visible = row
            .checked_sub(self.first)
            .filter(|row| *row < self.count)?;
        let y = self.new_top + u16::try_from(visible).ok()?;
        Some(Position::new(column, y))
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
        self.cache.forget_before(cell);
        self.blocks.clear();
        self.shown = None;
        self.committed_rows.resize(cell, 0);
    }

    /// Push everything on screen into the scrollback at the next frame, and start a clean region.
    pub(crate) fn request_clear(&mut self) {
        self.clear_requested = true;
    }

    /// The screen may no longer show what was drawn (a resize, a suspend): rows are typed rather
    /// than scrolled until a frame has drawn the tail again.
    pub(crate) fn forget_screen(&mut self) {
        self.shown = None;
    }

    /// Start the conversation over: the cells are new, and nothing of them is committed.
    /// The tool call drawn at `row` of the screen.
    /// The expandable card nearest the composer on screen.
    pub(crate) fn last_card(&self) -> Option<usize> {
        self.hits.last().map(|hit| hit.cell)
    }

    /// The expandable card drawn at `row` of the screen.
    pub(crate) fn card_at(&self, row: u16) -> Option<usize> {
        self.hits
            .iter()
            .find(|hit| hit.rows.contains(&row))
            .map(|hit| hit.cell)
    }

    /// Whether the terminal pushed `rows` rows of the tail into the scrollback behind our back (a
    /// resize shrank the screen), so they must not be drawn again.
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

    /// The conversation lost every cell from `len` on.
    pub(crate) fn truncated(&mut self, len: usize) {
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
    /// block of which `popup_rows` are a popup that covers the tail instead of pushing it up.
    pub(crate) fn plan(
        &mut self,
        view: &StageView<'_>,
        screen: Size,
        top: u16,
        height: u16,
        popup_rows: u16,
    ) -> Plan {
        let available = screen.height.saturating_sub(top);
        let full = height >= screen.height;
        let height = height.clamp(1, screen.height.max(1));
        if full {
            // A full-screen view: a picker, the onboarding, a child's conversation.
            self.shown = None;
            self.hits.clear();
            return Plan {
                commit_mode: Commit::None,
                scroll: 0,
                commit: 0,
                new_top: top,
                rows: Vec::new(),
                first: 0,
                count: 0,
                block: Rect::new(0, top, screen.width, available),
                blocks: Vec::new(),
                cards: Vec::new(),
                decorated: false,
                caret: None,
                full: true,
                whole: false,
            };
        }
        // Only the persistent part of the block (composer, status line, live tail) decides what
        // the conversation hands to the scrollback. The transient part (a popup, a picker, a
        // panel) sits on top of it and clips the tail from the top instead: the clipped rows are
        // still the stage's, and come back the moment it closes. Nothing is ever drawn under it.
        let base = height.saturating_sub(popup_rows).max(1);
        let tail = self.tail(view, screen.width);
        let decorated = view.focus.is_some() || tail.expanded;
        let rows_len = tail.rows.len();
        let wanted = rows_len.saturating_add(usize::from(base));
        let overflow = wanted.saturating_sub(usize::from(available));

        let (mut scroll, mut commit, mut new_top, mut first) = (0u16, 0usize, top, 0usize);
        let mut commit_mode = Commit::None;
        if std::mem::take(&mut self.clear_requested) {
            // Everything above the bottom block goes to the scrollback and the block moves up.
            let above = usize::from(top) + rows_len;
            scroll = u16::try_from(above).unwrap_or(u16::MAX).min(screen.height);
            commit = rows_len;
            new_top = 0;
            first = rows_len;
        } else if overflow > 0 && !decorated {
            scroll = u16::try_from(overflow).unwrap_or(u16::MAX);
            commit = overflow.saturating_sub(usize::from(top));
            new_top = top.saturating_sub(scroll);
            first = commit;
        }
        // A block taller than the rows below the region's top pushes the shell's own rows above
        // the region into the scrollback, as any program printing that much would.
        let short = height.saturating_sub(screen.height.saturating_sub(new_top));
        if short > 0 {
            let lift = short.min(new_top);
            scroll = scroll.saturating_add(lift);
            new_top -= lift;
        }
        let available = screen.height.saturating_sub(new_top);
        if commit > 0 {
            let on_screen = self.shown.as_ref().is_some_and(|shown| {
                shown.len() >= commit && shown[..commit] == tail.rows[..commit.min(rows_len)]
            });
            commit_mode = if on_screen {
                Commit::Scroll
            } else {
                Commit::Type
            };
        } else if scroll > 0 {
            commit_mode = Commit::Scroll;
        }
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
        let count_rows = u16::try_from(count).unwrap_or(u16::MAX);
        let block_y = new_top
            .saturating_add(count_rows)
            .min(screen.height.saturating_sub(height));
        Plan {
            commit_mode,
            scroll,
            commit,
            new_top,
            first,
            count,
            block: Rect::new(0, block_y, screen.width, height),
            blocks: tail.blocks,
            cards: tail.cards,
            decorated,
            caret: tail.caret,
            full: false,
            whole: !decorated && first == commit,
            rows: tail.rows,
        }
    }

    /// Carry out the scrolling and typing of `plan`, and leave the terminal's region where the
    /// plan puts it.
    pub(crate) fn commit<B>(&mut self, terminal: &mut Terminal<B>, plan: &Plan) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        let screen = terminal.size()?;
        let top = terminal.viewport_area.y;
        let area = Rect::new(0, plan.new_top, screen.width, screen.height - plan.new_top);
        match plan.commit_mode {
            Commit::None => {}
            Commit::Scroll => {
                write::scroll_screen_up(terminal.backend_mut(), screen.height, plan.scroll)?;
                // The viewport buffer keeps what is still on screen: rows move up with the screen.
                let lost = usize::from(plan.scroll).saturating_sub(usize::from(top));
                terminal.scroll_viewport(area, u16::try_from(lost).unwrap_or(u16::MAX));
            }
            Commit::Type => {
                write::type_rows(terminal.backend_mut(), &plan.rows, top, screen.width)?;
                // Typing scrolled the screen once for every row past its last; the plan wants
                // more (the rows under the tail, or everything for a cleared screen).
                let scrolled =
                    (usize::from(top) + plan.rows.len()).saturating_sub(usize::from(screen.height));
                let extra = usize::from(plan.scroll).saturating_sub(scrolled);
                write::scroll_screen_up(
                    terminal.backend_mut(),
                    screen.height,
                    u16::try_from(extra).unwrap_or(u16::MAX),
                )?;
                terminal.set_viewport_area(area);
            }
        }
        if terminal.viewport_area != area {
            terminal.set_viewport_area(area);
        }
        if plan.full {
            self.blocks.clear();
        } else if plan.commit > 0 || plan.scroll > 0 {
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
        Ok(())
    }

    /// Draw the tail of `plan` into `buffer`, whose first row is screen row `plan.new_top`.
    pub(crate) fn draw(&mut self, plan: &Plan, buffer: &mut Buffer) {
        self.hits.clear();
        if plan.full {
            self.displayed.clear();
            return;
        }
        let area = buffer.area;
        for (offset, row) in plan.rows[plan.first..plan.first + plan.count]
            .iter()
            .enumerate()
        {
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
            let start = range.start.max(plan.first);
            let end = range.end.min(plan.first + plan.count);
            if start >= end {
                continue;
            }
            let from = plan.new_top + u16::try_from(start - plan.first).unwrap_or(0);
            let to = plan.new_top + u16::try_from(end - plan.first).unwrap_or(0);
            self.hits.push(Hit {
                rows: from..to,
                cell: *cell,
            });
        }
        self.displayed = plan.rows[plan.first..plan.first + plan.count].to_vec();
        self.displayed_whole = plan.whole;
        self.shown = plan
            .whole
            .then(|| plan.rows[plan.first..plan.first + plan.count].to_vec());
    }
}

/// The tail of the conversation, as rows.
struct Tail {
    rows: Vec<Line<'static>>,
    blocks: Vec<Block>,
    cards: Vec<(std::ops::Range<usize>, usize)>,
    /// A card in it is expanded.
    expanded: bool,
    /// The rows of the focused prompt.
    focus_rows: Option<std::ops::Range<usize>>,
    caret: Option<(usize, u16)>,
}

impl Stage {
    fn tail(&mut self, view: &StageView<'_>, width: u16) -> Tail {
        let cells = view.cells;
        let boundary_cell = self.boundary.cell.min(cells.len());
        if boundary_cell != self.boundary.cell {
            self.boundary = Boundary {
                cell: boundary_cell,
                skip: 0,
                of: 0,
                fresh: self.boundary.fresh,
            };
        }
        let start = match view.focus {
            Some(focus) => boundary_cell.min(focus.selected.saturating_sub(1)),
            None => boundary_cell,
        };
        self.cache.forget_before(start);
        let theme = crate::style::component_theme();
        let mut tail = Tail {
            rows: Vec::new(),
            blocks: Vec::new(),
            cards: Vec::new(),
            expanded: false,
            focus_rows: None,
            caret: None,
        };
        for (index, cell) in cells.iter().enumerate().skip(start) {
            let plain = self.cache.rows(index, cell, width);
            // A cell with nothing to show leaves no blank row behind.
            if plain.lines.is_empty() && view.focus.is_none() {
                continue;
            }
            let separator = index > 0
                && !cell.is_stream_continuation()
                && !(index == boundary_cell && self.boundary.fresh);
            let mut lines = Vec::with_capacity(plain.lines.len() + 1);
            if separator {
                lines.push(Line::default());
            }
            let mut expandable = plain.expansion.is_some();
            let mut caret = None;
            let focused = view
                .focus
                .and_then(|focus| focus.rows(index, cell, &plain.lines, width, &theme));
            match focused {
                Some(focused) => {
                    caret = focused.caret;
                    expandable = false;
                    lines.extend(focused.lines);
                }
                None => lines.extend(plain.lines.iter().cloned()),
            }
            let total = lines.len();
            let skipped = if index == boundary_cell && start == boundary_cell {
                let skip = self.boundary.skip;
                if self.boundary.of != 0 && self.boundary.of != total {
                    skip * total / self.boundary.of
                } else {
                    skip
                }
                .min(total)
            } else {
                0
            };
            if skipped > 0 {
                lines.drain(..skipped);
                if self.boundary.of != total {
                    self.boundary.skip = skipped;
                    self.boundary.of = total;
                }
            }
            let first_row = tail.rows.len();
            if view.focus.is_some_and(|focus| focus.selected == index) {
                tail.focus_rows = Some(first_row..first_row + lines.len());
            }
            if let Some((row, column)) = caret {
                let row = (row + usize::from(separator)).saturating_sub(skipped);
                tail.caret = Some((first_row + row, column));
            }
            if expandable && !lines.is_empty() {
                tail.expanded |= plain.expansion == Some(true);
                tail.cards.push((first_row..first_row + lines.len(), index));
            }
            tail.blocks.push(Block {
                cell: index,
                rows: lines.len(),
                total,
            });
            tail.rows.extend(lines);
        }
        tail
    }
}

/// Move `boundary` past `rows` rows of the tail that `blocks` describe, and say what is left.
fn advance(boundary: Boundary, blocks: &[Block], rows: usize) -> (Boundary, Vec<Block>) {
    let mut left = rows;
    let mut next = boundary;
    let mut remaining = Vec::new();
    for (position, block) in blocks.iter().enumerate() {
        if left >= block.rows {
            left -= block.rows;
            next = Boundary {
                cell: block.cell + 1,
                skip: 0,
                of: 0,
                fresh: false,
            };
            continue;
        }
        next = Boundary {
            cell: block.cell,
            skip: block.total - block.rows + left,
            of: block.total,
            // A block that began without its blank row still does.
            fresh: boundary.fresh && position == 0,
        };
        remaining.push(Block {
            rows: block.rows - left,
            ..*block
        });
        remaining.extend_from_slice(&blocks[position + 1..]);
        return (next, remaining);
    }
    (next, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn blocks(rows: &[usize]) -> Vec<Block> {
        rows.iter()
            .enumerate()
            .map(|(cell, rows)| Block {
                cell,
                rows: *rows,
                total: *rows,
            })
            .collect()
    }

    #[test]
    fn advancing_past_whole_blocks_moves_to_the_next_cell() {
        let (boundary, left) = advance(Boundary::default(), &blocks(&[3, 2, 4]), 5);
        assert_eq!((boundary.cell, boundary.skip), (2, 0));
        assert_eq!(left.iter().map(|block| block.rows).collect::<Vec<_>>(), [4]);
    }

    #[test]
    fn advancing_into_a_block_remembers_how_far() {
        let (boundary, left) = advance(Boundary::default(), &blocks(&[3, 5]), 5);
        assert_eq!((boundary.cell, boundary.skip, boundary.of), (1, 2, 5));
        assert_eq!(left.iter().map(|block| block.rows).collect::<Vec<_>>(), [3]);
    }
}
