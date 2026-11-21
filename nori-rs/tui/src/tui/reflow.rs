//! How a terminal re-wraps the rows Nori drew when its width changes.
//!
//! Every row Nori writes ends by an explicit move or an erase, never by running past the last
//! column, so a terminal that reflows keeps each one as a line of its own and wraps it again at the
//! new width. The line runs through its last glyph, or to the end of the row when the row ends on
//! a colored background: the erase that cleared the rest of it kept the color, and terminals keep
//! colored cells as content. From the cells of the last frame Nori therefore knows how many rows
//! each of its rows became, and so where the region's first row went, without guessing.

use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Position;
use ratatui::style::Color;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthStr;

/// What a terminal does to the rows on its screen when its width changes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum WidthChange {
    /// Wraps every line again at the new width: tmux, Ghostty, kitty, iTerm2, WezTerm,
    /// Terminal.app and most others.
    #[default]
    Rewraps,
    /// Cuts every row at the new width and keeps it one row: xterm.
    Cuts,
}

impl WidthChange {
    /// What the terminal Nori runs in does, as far as its environment (`var`) says: xterm sets
    /// `XTERM_VERSION`, and a multiplexer running inside it, which reflows on its own, sets its
    /// own variable.
    pub(crate) fn of_terminal(var: impl Fn(&str) -> Option<String>) -> Self {
        let multiplexed = ["TMUX", "STY", "ZELLIJ"]
            .iter()
            .any(|name| var(name).is_some());
        if var("XTERM_VERSION").is_some() && !multiplexed {
            Self::Cuts
        } else {
            Self::Rewraps
        }
    }

    /// How many rows a row `extent` columns long becomes at `width` columns, given how many it
    /// takes when wrapped.
    pub(crate) fn rows(self, wrapped: usize) -> usize {
        match self {
            Self::Rewraps => wrapped,
            Self::Cuts => 1,
        }
    }
}

/// The rows of the last frame from the region's top down to the cursor, as the terminal shows
/// them after re-wrapping them to a new width.
pub(crate) struct Rewrapped {
    /// How many rows each frame row above the cursor's became, top first.
    pub(crate) rows: Vec<usize>,
    /// How many rows into its own row the cursor now sits.
    cursor_offset: usize,
}

impl Rewrapped {
    /// `frame` is the last frame (its area is the region), `cursor` where that frame left the
    /// cursor, `width` the new width of the screen, and `change` what the terminal did to it.
    pub(crate) fn of(frame: &Buffer, cursor: Position, width: u16, change: WidthChange) -> Self {
        let area = frame.area;
        let width = usize::from(width.max(1));
        let row_width = usize::from(area.width);
        let cells = |row: u16| {
            let start = usize::from(row) * row_width;
            frame.content.get(start..start + row_width).unwrap_or(&[])
        };
        let cursor_row = cursor.y.saturating_sub(area.y).min(area.height);
        let rows = (0..cursor_row)
            .map(|row| change.rows(wrapped_rows(cells(row), usize::MAX, width)))
            .collect();
        let cursor_offset = if cursor_row < area.height {
            let column = usize::from(cursor.x.saturating_sub(area.x));
            change.rows(wrapped_rows(cells(cursor_row), column, width)) - 1
        } else {
            0
        };
        Self {
            rows,
            cursor_offset,
        }
    }

    /// Rows between the region's first row and the cursor's, after re-wrapping.
    pub(crate) fn above_cursor(&self) -> usize {
        self.rows.iter().sum::<usize>() + self.cursor_offset
    }
}

/// How many rows the first `upto` columns of the row `cells` take once wrapped at `width`
/// columns, a wide glyph never split across two.
fn wrapped_rows(cells: &[Cell], upto: usize, width: usize) -> usize {
    let trailing_bg = cells.last().map_or(Color::Reset, |cell| cell.bg);
    // The same rule the frame writer follows: what it writes runs through the last cell that is
    // not a blank in the row's trailing background; the rest is erased in that background.
    let extent = if trailing_bg == Color::Reset {
        let mut extent = 0;
        let mut column = 0;
        while column < cells.len() {
            let cell = &cells[column];
            let width = cell_width(cell);
            if cell.symbol() != " " || cell.bg != trailing_bg || cell.modifier != Modifier::empty()
            {
                extent = column + width;
            }
            column += width;
        }
        extent
    } else {
        cells.len()
    };
    let end = extent.min(upto);
    let (mut rows, mut column, mut index) = (1, 0, 0);
    while index < end {
        let glyph = cells.get(index).map_or(1, cell_width);
        if column + glyph > width {
            rows += 1;
            column = 0;
        }
        column += glyph;
        index += glyph;
    }
    rows
}

/// Columns one cell's glyph takes (at least one, so a walk always advances).
fn cell_width(cell: &Cell) -> usize {
    cell.symbol().width().max(1)
}
