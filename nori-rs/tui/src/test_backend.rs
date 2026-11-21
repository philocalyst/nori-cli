use std::fmt::{self};
use std::io::Write;
use std::io::{self};

use ratatui::prelude::CrosstermBackend;

use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::backend::WindowSize;
use ratatui::buffer::Cell;
use ratatui::layout::Position;
use ratatui::layout::Size;

/// This wraps a CrosstermBackend and a vt100::Parser to mock
/// a "real" terminal.
///
/// Importantly, this wrapper avoids calling any crossterm methods
/// which write to stdout regardless of the writer. This includes:
/// - getting the terminal size
/// - getting the cursor position
pub struct VT100Backend {
    crossterm_backend: CrosstermBackend<vt100::Parser>,
    recording: Option<Vec<u8>>,
    scrollback: Option<Scrollback>,
}

impl VT100Backend {
    /// Creates a new `TestBackend` with the specified width and height.
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            crossterm_backend: CrosstermBackend::new(vt100::Parser::new(height, width, 0)),
            recording: None,
            scrollback: None,
        }
    }

    /// Like [`Self::new`], keeping the rows that scroll off the top of the screen the way a
    /// terminal with scrollback does (see [`Self::scrollback_rows`]).
    pub fn with_scrollback(width: u16, height: u16) -> Self {
        Self {
            scrollback: Some(Scrollback::default()),
            ..Self::new(width, height)
        }
    }

    /// The rows that scrolled off the top of the screen, oldest first, each as its text followed
    /// by the escape sequences that reproduce it (so equal rows are equal in style too).
    ///
    /// vt100 keeps scrollback only while no scroll region is set, but history is inserted
    /// through a region that starts at the top row, and terminals keep what scrolls out of the
    /// top of such a region. So the backend watches its own output for whatever scrolls a region
    /// that starts at the top row (a line feed or a wrapping character on its bottom margin,
    /// `ESC D`, `ESC E`, `CSI n S`), copies the rows about to leave before the parser scrolls
    /// them, and forgets them all on `CSI 3 J`.
    pub fn scrollback_rows(&self) -> &[(String, Vec<u8>)] {
        self.scrollback
            .as_ref()
            .map_or(&[], |scrollback| scrollback.rows.as_slice())
    }

    /// Feed `buf` to the parser, copying the rows that each scroll is about to push out first.
    fn feed_keeping_scrollback(&mut self, scrollback: &mut Scrollback, buf: &[u8]) {
        let mut fed = 0;
        for (index, &byte) in buf.iter().enumerate() {
            let scroll = match std::mem::take(&mut scrollback.escape) {
                Escape::Ground => match byte {
                    0x1b => {
                        scrollback.escape = Escape::Esc;
                        Scroll::None
                    }
                    b'\n' | 0x0b | 0x0c => Scroll::OnBottomMargin,
                    // A character that does not fit on the rest of the row wraps to the next one.
                    0x20..=0x7e | 0xc0..=0xff => {
                        let len = match byte {
                            0xc0..=0xdf => 2,
                            0xe0..=0xef => 3,
                            0xf0..=0xff => 4,
                            _ => 1,
                        };
                        let width = buf
                            .get(index..index + len)
                            .and_then(|bytes| std::str::from_utf8(bytes).ok())
                            .and_then(|text| text.chars().next())
                            .map_or(1, |c| {
                                unicode_width::UnicodeWidthChar::width(c).unwrap_or(1)
                            });
                        self.vt100_mut().process(&buf[fed..index]);
                        fed = index;
                        let (_, cols) = self.vt100().screen().size();
                        let (_, col) = self.vt100().screen().cursor_position();
                        if width > 0 && usize::from(col) + width > usize::from(cols) {
                            Scroll::OnBottomMargin
                        } else {
                            Scroll::None
                        }
                    }
                    _ => Scroll::None,
                },
                Escape::Esc => match byte {
                    b'[' => {
                        scrollback.escape = Escape::Csi(Vec::new());
                        Scroll::None
                    }
                    b']' => {
                        scrollback.escape = Escape::Osc;
                        Scroll::None
                    }
                    b'D' | b'E' => Scroll::OnBottomMargin,
                    b'c' => {
                        scrollback.margins = None;
                        Scroll::None
                    }
                    _ => Scroll::None,
                },
                Escape::Csi(mut params) if !(0x40..=0x7e).contains(&byte) => {
                    params.push(byte);
                    scrollback.escape = Escape::Csi(params);
                    Scroll::None
                }
                Escape::Csi(params) => {
                    let params = String::from_utf8_lossy(&params).into_owned();
                    let mut numbers = params
                        .split(';')
                        .map(|number| number.parse::<u16>().unwrap_or(0));
                    match byte {
                        b'r' => {
                            let (rows, _) = self.vt100().screen().size();
                            let top = numbers.next().unwrap_or(0).max(1) - 1;
                            let bottom = numbers
                                .next()
                                .filter(|bottom| *bottom > 0)
                                .unwrap_or(rows)
                                .min(rows)
                                - 1;
                            scrollback.margins = (top < bottom).then_some((top, bottom));
                            Scroll::None
                        }
                        b'S' => Scroll::Region(usize::from(numbers.next().unwrap_or(0).max(1))),
                        b'J' if params == "3" => {
                            scrollback.rows.clear();
                            Scroll::None
                        }
                        _ => Scroll::None,
                    }
                }
                Escape::Osc => {
                    scrollback.escape = match byte {
                        0x07 => Escape::Ground,
                        0x1b => Escape::OscEsc,
                        _ => Escape::Osc,
                    };
                    Scroll::None
                }
                Escape::OscEsc => {
                    if byte != b'\\' {
                        scrollback.escape = Escape::Osc;
                    }
                    Scroll::None
                }
            };
            if matches!(scroll, Scroll::None) {
                continue;
            }
            self.vt100_mut().process(&buf[fed..index]);
            fed = index;
            let screen = self.vt100().screen();
            let (rows, cols) = screen.size();
            let (top, bottom) = scrollback.margins.unwrap_or((0, rows - 1));
            let (row, _) = screen.cursor_position();
            let leaving = match scroll {
                Scroll::None => 0,
                Scroll::OnBottomMargin => usize::from(row == bottom),
                Scroll::Region(count) => count,
            };
            if top == 0 && leaving > 0 {
                scrollback.rows.extend(
                    screen
                        .rows(0, cols)
                        .zip(screen.rows_formatted(0, cols))
                        .take(leaving.min(usize::from(bottom) + 1)),
                );
            }
        }
        self.vt100_mut().process(&buf[fed..]);
    }

    /// Start keeping a copy of every byte written to the backend.
    pub fn start_recording(&mut self) {
        self.recording = Some(Vec::new());
    }

    /// The bytes written since [`Self::start_recording`].
    pub fn take_recording(&mut self) -> Vec<u8> {
        self.recording.take().unwrap_or_default()
    }

    /// Resize the way terminals that keep the cursor on screen do: when the screen gets shorter
    /// the rows at the top that the cursor would be pushed off by go into the scrollback; when it
    /// gets taller rows come back from the scrollback at the top (the cursor moves down with
    /// them), and any other row is blank. With `pull` false the terminal never pulls rows back:
    /// a taller screen only adds blank rows at the bottom.
    ///
    /// With `reflow` a new width wraps every line on the screen again, as tmux, Ghostty, kitty,
    /// iTerm2, WezTerm and Terminal.app do (see [`Self::rewrap`]); without it rows are cut to the
    /// new width and stay one row each, as xterm does.
    pub fn resize(&mut self, cols: u16, rows: u16, pull: bool, reflow: bool) {
        if reflow && cols != self.vt100().screen().size().1 {
            self.rewrap(cols);
        }
        let (old_rows, old_cols) = self.vt100().screen().size();
        let (cursor_row, cursor_col) = self.vt100().screen().cursor_position();
        let keep = old_cols.min(cols);
        let mut lines: Vec<Vec<u8>> = self.vt100().screen().rows_formatted(0, keep).collect();
        let mut texts: Vec<String> = self.vt100().screen().rows(0, keep).collect();
        // Which rows continue on the next one: kept, so a later width joins them again.
        let mut wrapped: Vec<bool> = (0..old_rows)
            .map(|row| self.vt100().screen().row_wrapped(row))
            .collect();
        let mut cursor_row = usize::from(cursor_row);
        let rows_wanted = usize::from(rows);
        if usize::from(rows) < lines.len() {
            let shift = (cursor_row + 1).saturating_sub(rows_wanted);
            let pushed: Vec<Vec<u8>> = lines.drain(..shift.min(texts.len())).collect();
            let pushed_texts = texts.drain(..shift.min(texts.len()));
            wrapped.drain(..shift.min(wrapped.len()));
            if let Some(scrollback) = self.scrollback.as_mut() {
                for (text, bytes) in pushed_texts.zip(pushed) {
                    scrollback.rows.push((text, bytes));
                }
            }
            cursor_row -= shift;
        } else if usize::from(rows) > lines.len() && pull {
            let pull = (usize::from(rows) - lines.len()).min(
                self.scrollback
                    .as_ref()
                    .map_or(0, |scrollback| scrollback.rows.len()),
            );
            if let Some(scrollback) = self.scrollback.as_mut() {
                let at = scrollback.rows.len() - pull;
                let (pulled_texts, pulled): (Vec<String>, Vec<Vec<u8>>) =
                    scrollback.rows.drain(at..).unzip();
                texts.splice(0..0, pulled_texts);
                lines.splice(0..0, pulled);
                wrapped.splice(0..0, std::iter::repeat_n(false, pull));
            }
            cursor_row += pull;
        }
        lines.truncate(rows_wanted);
        let mut parser = vt100::Parser::new(rows, cols, 0);
        for (row, bytes) in lines.iter().enumerate() {
            // A row that continues the one above is written right after it, so it wraps there.
            if !(row > 0 && wrapped.get(row - 1).copied().unwrap_or(false)) {
                parser.process(format!("\x1b[{};1H", row + 1).as_bytes());
            }
            parser.process(bytes);
        }
        let cursor_col = cursor_col.min(cols.saturating_sub(1));
        parser.process(
            format!(
                "\x1b[{};{}H",
                cursor_row.min(rows_wanted.saturating_sub(1)) + 1,
                cursor_col + 1
            )
            .as_bytes(),
        );
        *self.vt100_mut() = parser;
    }

    /// Wrap the lines on the screen again at `cols` columns, as a reflowing terminal does: rows
    /// it joined by wrapping are one line again, a line runs through its last glyph (or to the end
    /// of its last row when that ends on a colored background), and it takes as many rows as it
    /// needs at the new width. The screen grows to hold them; [`Self::resize`] then pushes what
    /// does not fit into the scrollback. The cursor stays on the glyph it was on.
    fn rewrap(&mut self, cols: u16) {
        let width = usize::from(cols.max(1));
        let screen = self.vt100().screen();
        let (old_rows, old_cols) = screen.size();
        let (cursor_row, cursor_col) = screen.cursor_position();
        // Each line as its glyphs (the bytes that draw one, and its width) and the column of it,
        // counted from the line's start, the cursor is on.
        let mut lines: Vec<(Vec<(Vec<u8>, usize)>, Option<usize>)> = Vec::new();
        let mut glyphs = Vec::new();
        let mut columns = 0;
        let mut cursor = None;
        for row in 0..old_rows {
            let cells: Vec<&vt100::Cell> = (0..old_cols)
                .filter_map(|col| screen.cell(row, col))
                .collect();
            let wraps = screen.row_wrapped(row);
            let extent = if wraps {
                cells.len()
            } else {
                cells
                    .iter()
                    .rposition(|cell| {
                        cell.has_contents() || cell.bgcolor() != vt100::Color::Default
                    })
                    .map_or(0, |last| last + if cells[last].is_wide() { 2 } else { 1 })
                    .min(cells.len())
            };
            if row == cursor_row {
                cursor = Some(columns + usize::from(cursor_col));
            }
            for cell in &cells[..extent] {
                if cell.is_wide_continuation() {
                    continue;
                }
                let glyph = if cell.is_wide() { 2 } else { 1 };
                glyphs.push((cell_bytes(cell), glyph));
                columns += glyph;
            }
            if !wraps {
                lines.push((std::mem::take(&mut glyphs), cursor.take()));
                columns = 0;
            }
        }
        if !glyphs.is_empty() || cursor.is_some() {
            lines.push((glyphs, cursor));
        }
        // Where every glyph goes at the new width, a wide one never split across two rows.
        let mut placed: Vec<Vec<(usize, usize)>> = Vec::with_capacity(lines.len());
        let mut heights = Vec::with_capacity(lines.len());
        for (glyphs, _) in &lines {
            let (mut row, mut column) = (0, 0);
            let mut at = Vec::with_capacity(glyphs.len());
            for (_, glyph) in glyphs {
                if column + glyph > width {
                    row += 1;
                    column = 0;
                }
                at.push((row, column));
                column += glyph;
            }
            placed.push(at);
            heights.push(row + 1);
        }
        let total: usize = heights.iter().sum();
        let height = u16::try_from(total.max(usize::from(old_rows)).max(1)).unwrap_or(u16::MAX);
        let mut parser = vt100::Parser::new(height, cols, 0);
        let mut first_row = 0;
        let mut new_cursor = (0, 0);
        for (((glyphs, cursor), at), rows) in lines.iter().zip(&placed).zip(&heights) {
            parser.process(format!("\x1b[{};1H", first_row + 1).as_bytes());
            for (bytes, _) in glyphs {
                parser.process(bytes);
            }
            if let Some(cursor) = cursor {
                // The glyph the cursor was on, or as far past the line's end as it was.
                let mut columns = 0;
                let mut spot = None;
                for ((_, glyph), &(row, column)) in glyphs.iter().zip(at) {
                    if columns + glyph > *cursor {
                        spot = Some((row, column));
                        break;
                    }
                    columns += glyph;
                }
                let (row, column) = spot.unwrap_or_else(|| {
                    let (row, column) = at
                        .last()
                        .zip(glyphs.last())
                        .map_or((0, 0), |(&(row, column), (_, glyph))| (row, column + glyph));
                    (row, (column + cursor - columns).min(width - 1))
                });
                new_cursor = (first_row + row.min(rows - 1), column);
            }
            first_row += rows;
        }
        parser.process(format!("\x1b[{};{}H", new_cursor.0 + 1, new_cursor.1 + 1).as_bytes());
        *self.vt100_mut() = parser;
    }

    pub fn vt100_mut(&mut self) -> &mut vt100::Parser {
        self.crossterm_backend.writer_mut()
    }

    pub fn vt100(&self) -> &vt100::Parser {
        self.crossterm_backend.writer()
    }
}

/// Rows that scrolled off the top of the screen, and what is needed to see the next ones go.
#[derive(Default)]
struct Scrollback {
    rows: Vec<(String, Vec<u8>)>,
    /// The scroll region's top and bottom rows, when one is set.
    margins: Option<(u16, u16)>,
    escape: Escape,
}

/// What the byte being written does to the rows at the top of the screen.
enum Scroll {
    None,
    /// Scrolls the region up a row if the cursor is on its bottom margin.
    OnBottomMargin,
    /// Scrolls the region up this many rows.
    Region(usize),
}

/// Where the backend's output is inside an escape sequence.
#[derive(Default)]
enum Escape {
    #[default]
    Ground,
    Esc,
    Csi(Vec<u8>),
    Osc,
    OscEsc,
}

impl Write for VT100Backend {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(recording) = self.recording.as_mut() {
            recording.extend_from_slice(buf);
        }
        match self.scrollback.take() {
            Some(mut scrollback) => {
                self.feed_keeping_scrollback(&mut scrollback, buf);
                self.scrollback = Some(scrollback);
            }
            None => self.vt100_mut().process(buf),
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.crossterm_backend.writer_mut().flush()
    }
}

impl fmt::Display for VT100Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.crossterm_backend.writer().screen().contents())
    }
}

impl Backend for VT100Backend {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        self.crossterm_backend.draw(content)?;
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.crossterm_backend.hide_cursor()?;
        Ok(())
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.crossterm_backend.show_cursor()?;
        Ok(())
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        // vt100 says (row, column); a position is (x, y).
        let (row, column) = self.vt100().screen().cursor_position();
        Ok(Position::new(column, row))
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.crossterm_backend.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.crossterm_backend.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.crossterm_backend.clear_region(clear_type)
    }

    fn append_lines(&mut self, line_count: u16) -> io::Result<()> {
        self.crossterm_backend.append_lines(line_count)
    }

    fn size(&self) -> io::Result<Size> {
        let (rows, cols) = self.vt100().screen().size();
        Ok(Size::new(cols, rows))
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        Ok(WindowSize {
            columns_rows: self.vt100().screen().size().into(),
            // Arbitrary size, we don't rely on this in testing.
            pixels: Size {
                width: 640,
                height: 480,
            },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.crossterm_backend.writer_mut().flush()
    }

    fn scroll_region_up(&mut self, region: std::ops::Range<u16>, scroll_by: u16) -> io::Result<()> {
        self.crossterm_backend.scroll_region_up(region, scroll_by)
    }

    fn scroll_region_down(
        &mut self,
        region: std::ops::Range<u16>,
        scroll_by: u16,
    ) -> io::Result<()> {
        self.crossterm_backend.scroll_region_down(region, scroll_by)
    }
}

/// The bytes that draw `cell` with its colors and attributes, from a reset.
fn cell_bytes(cell: &vt100::Cell) -> Vec<u8> {
    let mut sgr = vec!["0".to_owned()];
    for (on, code) in [
        (cell.bold(), "1"),
        (cell.dim(), "2"),
        (cell.italic(), "3"),
        (cell.underline(), "4"),
        (cell.inverse(), "7"),
    ] {
        if on {
            sgr.push(code.to_owned());
        }
    }
    for (color, base) in [(cell.fgcolor(), 38), (cell.bgcolor(), 48)] {
        match color {
            vt100::Color::Default => {}
            vt100::Color::Idx(index) => sgr.push(format!("{base};5;{index}")),
            vt100::Color::Rgb(red, green, blue) => {
                sgr.push(format!("{base};2;{red};{green};{blue}"));
            }
        }
    }
    let contents = if cell.has_contents() {
        cell.contents()
    } else {
        " "
    };
    format!("\x1b[{}m{contents}", sgr.join(";")).into_bytes()
}
