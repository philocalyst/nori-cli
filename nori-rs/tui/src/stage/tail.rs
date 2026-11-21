//! The tail: every row of the conversation from the *boundary*, the first row that is not in the
//! scrollback, as hard-wrapped rows, and how far the rows that settled reach.

use ratatui::text::Line;

use super::Stage;
use super::StageView;

/// The first row of the conversation that is not in the scrollback.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct Boundary {
    /// The cell it is in.
    pub(super) cell: usize,
    /// How many rows of that cell (counting the blank row before it) are already committed.
    pub(super) skip: usize,
    /// How many rows that cell had when `skip` was counted, so a width change can scale it.
    pub(super) of: usize,
    /// The screen was just cleared: no blank row separates the next cell from what is above.
    pub(super) fresh: bool,
}

/// The rows one cell contributes to the tail.
#[derive(Clone, Copy, Debug)]
pub(super) struct Block {
    pub(super) cell: usize,
    /// Rows left in the tail, after the ones already committed.
    pub(super) rows: usize,
    /// Rows the block has in all.
    pub(super) total: usize,
}

/// The tail of the conversation, as rows: every row from the boundary on.
pub(super) struct Tail {
    pub(super) rows: Vec<Line<'static>>,
    pub(super) blocks: Vec<Block>,
    pub(super) cards: Vec<(std::ops::Range<usize>, usize)>,
    /// A card in it is expanded.
    pub(super) expanded: bool,
    /// The rows of the focused prompt.
    pub(super) focus_rows: Option<std::ops::Range<usize>>,
    /// Where the cursor of a prompt edited in place is: `(row, column)` of the tail.
    pub(super) focus_cursor: Option<(usize, u16)>,
    /// How many of the first rows belong to cells that will not change any more (nothing before
    /// them is still running or animating): rows the terminal can have for good.
    pub(super) settled: usize,
    /// The rows start at the boundary (a focused prompt can start them before it).
    pub(super) from_boundary: bool,
}

impl Stage {
    pub(super) fn tail(&mut self, view: &StageView<'_>, width: u16) -> Tail {
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
        let mut tail = Tail {
            rows: Vec::new(),
            blocks: Vec::new(),
            cards: Vec::new(),
            expanded: false,
            focus_rows: None,
            focus_cursor: None,
            settled: 0,
            from_boundary: start == boundary_cell,
        };
        // Rows settle in order: the first cell that may still change holds back every one after.
        let mut settling = true;
        for (index, cell) in cells.iter().enumerate().skip(start) {
            let plain = self.cache.rows(index, cell, width);
            // A cell with nothing to show leaves no blank row behind, focused or not: the rows a
            // focus draws over must be the rows that were printed.
            if plain.lines.is_empty() && !view.focus.is_some_and(|focus| focus.selected == index) {
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
            let focused = view
                .focus
                .and_then(|focus| focus.rows(index, &plain.lines, width));
            // The cursor's row among this cell's rows, the separator counted.
            let mut cursor = None;
            match focused {
                Some(focused) => {
                    expandable = false;
                    cursor = focused
                        .cursor
                        .map(|(row, column)| (row + usize::from(separator), column));
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
                tail.focus_cursor = cursor.and_then(|(row, column)| {
                    Some((first_row + row.checked_sub(skipped)?, column))
                });
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
            settling &= cell.settled() && !(view.open && index + 1 == cells.len());
            if settling {
                tail.settled += lines.len();
            }
            tail.rows.extend(lines);
        }
        tail
    }
}

/// Move `boundary` past `rows` rows of the tail that `blocks` describe, and say what is left.
pub(super) fn advance(boundary: Boundary, blocks: &[Block], rows: usize) -> (Boundary, Vec<Block>) {
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
