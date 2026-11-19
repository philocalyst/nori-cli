//! A grid of tiles: the shape a run of file reads, searches and listings takes instead of a list.
//!
//! Tiles are one row each, equal in width, two to four to a row by how wide the terminal is, laid
//! out left to right. Each is `◈ dir/file.rs:10–40`: the directory faint, the file name bright,
//! the range muted; a path too long for its tile loses its head, never its file name.

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::chrome::Chrome;
use super::chrome::GUTTER;
use super::chrome::columns_for;
use super::chrome::path_spans;
use super::chrome::side_by_side;
use super::chrome::spans_width;
use super::chrome::truncate_head;
use super::chrome::truncate_spans;
use super::chrome::truncate_tail;

/// The narrowest a tile is allowed to be before the grid drops a column.
const MIN_TILE_WIDTH: usize = 26;
/// Tiles never sit in more than this many columns, however wide the terminal.
const MAX_COLUMNS: usize = 4;
const GAP: usize = 1;

/// One tile of a grid.
pub(crate) struct Tile {
    pub(crate) glyph: Span<'static>,
    /// A path or a short phrase.
    pub(crate) head: String,
    /// Whether `head` is a path (cut from the front, directory faint) or a phrase (cut from the
    /// end).
    pub(crate) path_like: bool,
    /// What follows it, muted: `:10–40`, ` in src/`.
    pub(crate) tail: String,
    /// A few rows of what the tile is about (a read's first lines, a search's matches), drawn
    /// under it on the well. Empty for a tile that is only a label.
    pub(crate) excerpt: Vec<Vec<Span<'static>>>,
}

/// The grid for `tiles` at `width`, indented under a header. At most `max_rows` rows are drawn;
/// the tile that would have begun the next row says how many were left out.
pub(crate) fn grid(
    chrome: &Chrome,
    tiles: Vec<Tile>,
    width: usize,
    max_rows: Option<usize>,
) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(GUTTER).max(8);
    let columns = columns_for(inner, MIN_TILE_WIDTH, GAP, MAX_COLUMNS);
    let tile_width = (inner - GAP * (columns - 1)) / columns;
    let capacity = max_rows.map(|rows| rows * columns);
    let overflow = capacity.filter(|capacity| tiles.len() > *capacity);
    let shown = overflow.map_or(tiles.len(), |capacity| capacity - 1);
    let hidden = tiles.len() - shown;

    let mut cells: Vec<Vec<Line<'static>>> = tiles
        .into_iter()
        .take(shown)
        .map(|tile| tile_column(chrome, tile, tile_width))
        .collect();
    if overflow.is_some() {
        let more = Tile {
            glyph: Span::styled("⋯", chrome.faint()),
            head: format!("+{hidden} more"),
            path_like: false,
            tail: String::new(),
            excerpt: Vec::new(),
        };
        cells.push(tile_column(chrome, more, tile_width));
    }

    let mut lines = Vec::new();
    for band in cells.chunks(columns) {
        for line in side_by_side(band.to_vec(), tile_width, GAP) {
            let mut spans = vec![Span::raw(" ".repeat(GUTTER))];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
    lines
}

/// A tile as its column: its label row, then its excerpt rows.
fn tile_column(chrome: &Chrome, tile: Tile, width: usize) -> Vec<Line<'static>> {
    let excerpt = tile.excerpt.clone();
    let mut column = vec![Line::from(tile_spans(chrome, tile, width))];
    let well = chrome.theme.code_well;
    for row in excerpt {
        let mut spans = vec![if chrome.has_surfaces() {
            Span::raw(" ")
        } else {
            Span::styled("▏", chrome.faint())
        }];
        spans.extend(truncate_spans(row, width.saturating_sub(2)));
        let used = spans_width(&spans);
        if used < width {
            spans.push(Span::raw(" ".repeat(width - used)));
        }
        if chrome.has_surfaces() {
            for span in &mut spans {
                span.style = well.patch(span.style);
            }
        }
        column.push(Line::from(spans));
    }
    column
}

fn tile_spans(chrome: &Chrome, tile: Tile, width: usize) -> Vec<Span<'static>> {
    let surface: Style = chrome.theme.card;
    let edge = if chrome.has_surfaces() {
        Span::raw(" ")
    } else {
        Span::styled("▏", chrome.faint())
    };
    let tail_width = tile.tail.width();
    // Edge, glyph, space on the left; one column of air on the right.
    let room = width.saturating_sub(3 + tail_width + 1);
    let mut spans = vec![edge, tile.glyph, Span::raw(" ")];
    if tile.path_like {
        spans.extend(path_spans(&truncate_head(&tile.head, room), chrome));
    } else {
        spans.push(Span::styled(
            truncate_tail(&tile.head, room),
            chrome.secondary(),
        ));
    }
    if !tile.tail.is_empty() {
        spans.push(Span::styled(tile.tail, chrome.muted()));
    }
    let used = spans_width(&spans);
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    for span in &mut spans {
        span.style = surface.patch(span.style);
    }
    spans
}
