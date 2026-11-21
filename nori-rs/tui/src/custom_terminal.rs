// This is derived from `ratatui::Terminal`, which is licensed under the following terms:
//
// The MIT License (MIT)
// Copyright (c) 2016-2022 Florian Dehau
// Copyright (c) 2023-2025 The Ratatui Developers
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
use std::io;
use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Color as CrosstermColor;
use crossterm::style::Colors;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetColors;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use ratatui::backend::Backend;
use ratatui::backend::ClearType;
use ratatui::buffer::Buffer;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Position;
use ratatui::layout::Rect;
use ratatui::layout::Size;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::widgets::WidgetRef;

pub(crate) const fn crossterm_color(color: Color) -> CrosstermColor {
    match color {
        Color::Reset => CrosstermColor::Reset,
        Color::Black => CrosstermColor::Black,
        Color::Red => CrosstermColor::DarkRed,
        Color::Green => CrosstermColor::DarkGreen,
        Color::Yellow => CrosstermColor::DarkYellow,
        Color::Blue => CrosstermColor::DarkBlue,
        Color::Magenta => CrosstermColor::DarkMagenta,
        Color::Cyan => CrosstermColor::DarkCyan,
        Color::Gray => CrosstermColor::Grey,
        Color::DarkGray => CrosstermColor::DarkGrey,
        Color::LightRed => CrosstermColor::Red,
        Color::LightGreen => CrosstermColor::Green,
        Color::LightYellow => CrosstermColor::Yellow,
        Color::LightBlue => CrosstermColor::Blue,
        Color::LightMagenta => CrosstermColor::Magenta,
        Color::LightCyan => CrosstermColor::Cyan,
        Color::White => CrosstermColor::White,
        Color::Rgb(red, green, blue) => CrosstermColor::Rgb {
            r: red,
            g: green,
            b: blue,
        },
        Color::Indexed(index) => CrosstermColor::AnsiValue(index),
    }
}

#[derive(Debug, Hash)]
pub struct Frame<'a> {
    /// Where should the cursor be after drawing this frame?
    ///
    /// If `None`, the cursor is hidden and its position is controlled by the backend. If `Some((x,
    /// y))`, the cursor is shown and placed at `(x, y)` after the call to `Terminal::draw()`.
    pub(crate) cursor_position: Option<Position>,

    /// The area of the viewport
    pub(crate) viewport_area: Rect,

    /// The buffer that is used to draw the current frame
    pub(crate) buffer: &'a mut Buffer,
}

impl Frame<'_> {
    /// The area of the current frame
    ///
    /// This is guaranteed not to change during rendering, so may be called multiple times.
    ///
    /// If your app listens for a resize event from the backend, it should ignore the values from
    /// the event for any calculations that are used to render the current frame and use this value
    /// instead as this is the area of the buffer that is used to render the current frame.
    pub const fn area(&self) -> Rect {
        self.viewport_area
    }

    /// Render a [`WidgetRef`] to the current buffer using [`WidgetRef::render_ref`].
    ///
    /// Usually the area argument is the size of the current frame or a sub-area of the current
    /// frame (which can be obtained using [`Layout`] to split the total area).
    #[allow(clippy::needless_pass_by_value)]
    pub fn render_widget_ref<W: WidgetRef>(&mut self, widget: W, area: Rect) {
        widget.render_ref(area, self.buffer);
    }

    /// After drawing this frame, make the cursor visible and put it at the specified (x, y)
    /// coordinates. If this method is not called, the cursor will be hidden.
    ///
    /// Note that this will interfere with calls to [`Terminal::hide_cursor`],
    /// [`Terminal::show_cursor`], and [`Terminal::set_cursor_position`]. Pick one of the APIs and
    /// stick with it.
    ///
    /// [`Terminal::hide_cursor`]: crate::Terminal::hide_cursor
    /// [`Terminal::show_cursor`]: crate::Terminal::show_cursor
    /// [`Terminal::set_cursor_position`]: crate::Terminal::set_cursor_position
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) {
        self.cursor_position = Some(position.into());
    }

    /// Gets the buffer that this `Frame` draws into as a mutable reference.
    pub fn buffer_mut(&mut self) -> &mut Buffer {
        self.buffer
    }
}

#[derive(Debug, Default, Clone, Eq, PartialEq, Hash)]
pub struct Terminal<B>
where
    B: Backend<Error = io::Error> + Write,
{
    /// The backend used to interface with the terminal
    backend: B,
    /// Holds the results of the current and previous draw calls. The two are compared at the end
    /// of each draw pass to output the necessary updates to the terminal
    buffers: [Buffer; 2],
    /// Index of the current buffer in the previous array
    current: usize,
    /// Whether the cursor is currently hidden
    pub hidden_cursor: bool,
    /// Area of the viewport
    pub viewport_area: Rect,
    /// Last known size of the terminal. Used to detect if the internal buffers have to be resized.
    pub last_known_screen_size: Size,
    /// Last known position of the cursor. Used to find the new area when the viewport is inlined
    /// and the terminal resized.
    pub last_known_cursor_pos: Position,
    /// The terminal may no longer show the previous buffer, so the next frame repaints every row
    /// instead of only the rows that changed.
    repaint_all: bool,
}

impl<B> Drop for Terminal<B>
where
    B: Backend<Error = io::Error>,
    B: Write,
{
    #[allow(clippy::print_stderr)]
    fn drop(&mut self) {
        // Attempt to restore the cursor state
        if self.hidden_cursor
            && let Err(err) = self.show_cursor()
        {
            eprintln!("Failed to show the cursor: {err}");
        }
    }
}

impl<B> Terminal<B>
where
    B: Backend<Error = io::Error>,
    B: Write,
{
    /// Creates a new [`Terminal`] with the given [`Backend`] and [`TerminalOptions`].
    pub fn with_options(mut backend: B) -> io::Result<Self> {
        let screen_size = backend.size()?;
        let cursor_pos = backend.get_cursor_position()?;
        Ok(Self {
            backend,
            buffers: [Buffer::empty(Rect::ZERO), Buffer::empty(Rect::ZERO)],
            current: 0,
            hidden_cursor: false,
            viewport_area: Rect::new(0, cursor_pos.y, 0, 0),
            last_known_screen_size: screen_size,
            last_known_cursor_pos: cursor_pos,
            repaint_all: true,
        })
    }

    /// Get a Frame object which provides a consistent view into the terminal state for rendering.
    pub fn get_frame(&mut self) -> Frame<'_> {
        Frame {
            cursor_position: None,
            viewport_area: self.viewport_area,
            buffer: self.current_buffer_mut(),
        }
    }

    /// Gets the current buffer as a mutable reference.
    fn current_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[self.current]
    }

    /// Gets the previous buffer as a mutable reference.
    fn previous_buffer_mut(&mut self) -> &mut Buffer {
        &mut self.buffers[1 - self.current]
    }

    /// The cells of the last frame drawn, which the screen shows unless something moved it.
    pub(crate) fn last_frame(&self) -> &Buffer {
        &self.buffers[1 - self.current]
    }

    /// Gets the backend
    pub const fn backend(&self) -> &B {
        &self.backend
    }

    /// Gets the backend as a mutable reference
    pub fn backend_mut(&mut self) -> &mut B {
        &mut self.backend
    }

    /// Obtains a difference between the previous and the current buffer and passes it to the
    /// current backend for drawing.
    pub fn flush(&mut self) -> io::Result<()> {
        let repaint_all = std::mem::take(&mut self.repaint_all);
        let mut sink = TerminalSink::new(&mut self.backend);
        let last_put = diff_buffers(
            &self.buffers[1 - self.current],
            &self.buffers[self.current],
            repaint_all,
            &mut sink,
        )?;
        sink.finish()?;
        if let Some(position) = last_put {
            self.last_known_cursor_pos = position;
        }
        Ok(())
    }

    /// Updates the Terminal so that internal buffers match the requested area.
    ///
    /// Requested area will be saved to remain consistent when rendering. This leads to a full clear
    /// of the screen.
    pub fn resize(&mut self, screen_size: Size) -> io::Result<()> {
        self.last_known_screen_size = screen_size;
        Ok(())
    }

    /// Sets the viewport area.
    pub fn set_viewport_area(&mut self, area: Rect) {
        self.current_buffer_mut().resize(area);
        self.previous_buffer_mut().resize(area);
        self.viewport_area = area;
        self.repaint_all = true;
    }

    /// Queries the backend for size and resizes if it doesn't match the previous size.
    pub fn autoresize(&mut self) -> io::Result<()> {
        let screen_size = self.size()?;
        if screen_size != self.last_known_screen_size {
            self.resize(screen_size)?;
        }
        Ok(())
    }

    /// Draws a single frame to the terminal.
    ///
    /// Returns a [`CompletedFrame`] if successful, otherwise a [`std::io::Error`].
    ///
    /// If the render callback passed to this method can fail, use [`try_draw`] instead.
    ///
    /// Applications should call `draw` or [`try_draw`] in a loop to continuously render the
    /// terminal. These methods are the main entry points for drawing to the terminal.
    ///
    /// [`try_draw`]: Terminal::try_draw
    ///
    /// This method will:
    ///
    /// - autoresize the terminal if necessary
    /// - call the render callback, passing it a [`Frame`] reference to render to
    /// - flush the current internal state by copying the current buffer to the backend
    /// - move the cursor to the last known position if it was set during the rendering closure
    ///
    /// The render callback should fully render the entire frame when called, including areas that
    /// are unchanged from the previous frame. This is because each frame is compared to the
    /// previous frame to determine what has changed, and only the changes are written to the
    /// terminal. If the render callback does not fully render the frame, the terminal will not be
    /// in a consistent state.
    pub fn draw<F>(&mut self, render_callback: F) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        self.autoresize()?;
        self.draw_in(self.viewport_area, None, render_callback)
    }

    /// Draws a frame whose widgets see `area` (inside the viewport) as their frame. The cursor is
    /// shown where the frame puts it; without one it is hidden and left at `park`, so the row it
    /// is on is known to whoever must find the screen again after a resize.
    ///
    /// The render callback should fully render the entire viewport when called, including areas
    /// that are unchanged from the previous frame, because each frame is compared with the
    /// previous one and only the differences are written. The caller decides the screen the frame
    /// is for ([`Self::resize`]): a resize while it is drawn is the next frame's to find.
    pub fn draw_in<F>(
        &mut self,
        area: Rect,
        park: Option<Position>,
        render_callback: F,
    ) -> io::Result<()>
    where
        F: FnOnce(&mut Frame),
    {
        crate::hyperlink::begin_frame();
        let mut frame = self.get_frame();
        frame.viewport_area = area;

        render_callback(&mut frame);

        // We can't change the cursor position right away because we have to flush the frame to
        // stdout first. But we also can't keep the frame around, since it holds a &mut to
        // Buffer. Thus, we're taking the important data out of the Frame and dropping it.
        let cursor_position = frame.cursor_position;

        // Draw to stdout
        self.flush()?;

        match cursor_position {
            None => {
                self.hide_cursor()?;
                if let Some(park) = park {
                    self.set_cursor_position(park)?;
                }
            }
            Some(position) => {
                self.show_cursor()?;
                self.set_cursor_position(position)?;
            }
        }

        self.swap_buffers();

        Backend::flush(&mut self.backend)?;

        Ok(())
    }

    /// Hides the cursor.
    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.backend.hide_cursor()?;
        self.hidden_cursor = true;
        Ok(())
    }

    /// Shows the cursor.
    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.backend.show_cursor()?;
        self.hidden_cursor = false;
        Ok(())
    }

    /// Gets the current cursor position.
    ///
    /// This is the position of the cursor after the last draw call.
    #[allow(dead_code)]
    pub fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.backend.get_cursor_position()
    }

    /// Sets the cursor position.
    pub fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let position = position.into();
        self.backend.set_cursor_position(position)?;
        self.last_known_cursor_pos = position;
        Ok(())
    }

    /// The terminal no longer shows what the previous frame drew: the next frame repaints
    /// every row.
    pub(crate) fn invalidate(&mut self) {
        self.previous_buffer_mut().reset();
        self.repaint_all = true;
    }

    /// Clear the terminal and force a full redraw on the next draw call.
    pub fn clear(&mut self) -> io::Result<()> {
        if self.viewport_area.is_empty() {
            return Ok(());
        }
        // Row by row: erasing "from the cursor down" at the top of the screen is read by some
        // terminals (tmux) as clearing the screen, and they push it into the scrollback.
        for row in self.viewport_area.top()..self.viewport_area.bottom() {
            self.backend.set_cursor_position(Position::new(0, row))?;
            self.backend.clear_region(ClearType::CurrentLine)?;
        }
        self.backend
            .set_cursor_position(self.viewport_area.as_position())?;
        // Reset the back buffer to make sure the next update will redraw everything.
        self.previous_buffer_mut().reset();
        self.repaint_all = true;
        Ok(())
    }

    /// The screen scrolled up and the viewport is now `area`: what the previous frame drew is
    /// still on screen, moved up by `lost` rows of the viewport (the rows that passed its top),
    /// with blank rows where the screen scrolled in.
    pub(crate) fn scroll_viewport(&mut self, area: Rect, lost: u16) {
        let width = usize::from(self.viewport_area.width);
        let old_height = usize::from(self.viewport_area.height);
        let previous = std::mem::take(&mut self.previous_buffer_mut().content);
        let repaint_all = self.repaint_all;
        self.set_viewport_area(area);
        // `set_viewport_area` asked for every row to be repainted; the rows that moved are known.
        self.repaint_all = repaint_all;
        let lost = usize::from(lost);
        if width == usize::from(area.width) && lost < old_height {
            let kept = (old_height - lost).min(usize::from(area.height));
            let target = &mut self.previous_buffer_mut().content;
            target[..kept * width].clone_from_slice(&previous[lost * width..(lost + kept) * width]);
        } else {
            self.repaint_all = true;
        }
    }

    /// The viewport grows up over rows already on screen and is now `area`: the rows it had are
    /// its last `area.height - seeds.len()`, and `seeds` says what the screen shows on each row
    /// it grew over, so the next frame writes only what differs. A row it cannot say (`None`)
    /// makes the next frame write every row.
    pub(crate) fn reclaim_viewport(&mut self, area: Rect, seeds: &[Option<&Line<'static>>]) {
        let width = usize::from(self.viewport_area.width);
        let old_height = usize::from(self.viewport_area.height);
        let previous = std::mem::take(&mut self.previous_buffer_mut().content);
        let repaint_all = self.repaint_all;
        self.set_viewport_area(area);
        self.repaint_all = repaint_all;
        let added = seeds.len();
        if width != usize::from(area.width) || seeds.iter().any(Option::is_none) {
            self.repaint_all = true;
            return;
        }
        let kept = old_height.min(usize::from(area.height).saturating_sub(added));
        let target = self.previous_buffer_mut();
        target.content[added * width..(added + kept) * width]
            .clone_from_slice(&previous[..kept * width]);
        for (row, line) in seeds.iter().flatten().enumerate() {
            let y = area.y + u16::try_from(row).unwrap_or(u16::MAX);
            target.set_style(Rect::new(area.x, y, area.width, 1), line.style);
            target.set_line(area.x, y, line, area.width);
        }
    }

    /// Clears the inactive buffer and swaps it with the current buffer
    pub fn swap_buffers(&mut self) {
        self.previous_buffer_mut().reset();
        self.current = 1 - self.current;
    }

    /// Queries the real size of the backend.
    pub fn size(&self) -> io::Result<Size> {
        self.backend.size()
    }
}

use ratatui::buffer::Cell;
use unicode_width::UnicodeWidthStr;

/// Where the differences between two frames go.
trait DiffSink {
    fn put(&mut self, x: u16, y: u16, cell: &Cell) -> io::Result<()>;
    fn clear_to_end(&mut self, x: u16, y: u16, bg: Color) -> io::Result<()>;
    /// The hyperlink the cells put next belong to (`None`: none).
    fn link(&mut self, _target: Option<&str>) -> io::Result<()> {
        Ok(())
    }
}

/// Display width of one buffer cell's symbol. Nearly every cell is a single ASCII character, which
/// is one column; only the rest needs the Unicode tables.
fn symbol_width(symbol: &str) -> usize {
    match symbol.as_bytes() {
        [byte] if (0x20..0x7f).contains(byte) => 1,
        _ => symbol.width(),
    }
}

/// Send the differences between `a` (what the terminal shows) and `b` (the frame to show) to
/// `sink`, row by row. A row that did not change is skipped without looking at its cells, unless
/// `repaint_all` says the terminal can no longer be trusted to show `a`.
///
/// Returns the position of the last cell that was put.
fn diff_buffers(
    a: &Buffer,
    b: &Buffer,
    repaint_all: bool,
    sink: &mut impl DiffSink,
) -> io::Result<Option<Position>> {
    let width = a.area.width as usize;
    let previous_buffer = &a.content;
    let next_buffer = &b.content;
    let mut last_put = None;
    if width == 0 {
        return Ok(None);
    }

    for y in 0..a.area.height {
        let row_start = y as usize * width;
        let previous_row = &previous_buffer[row_start..row_start + width];
        let row = &next_buffer[row_start..row_start + width];
        if !repaint_all && previous_row == row {
            continue;
        }
        let bg = row.last().map(|cell| cell.bg).unwrap_or(Color::Reset);

        // Scan the row to find the rightmost column that still matters: any non-space glyph,
        // any cell whose bg differs from the row’s trailing bg, or any cell with modifiers.
        // Multi-width glyphs extend that region through their full displayed width.
        let mut last_nonblank_column = 0usize;
        let mut column = 0usize;
        while column < row.len() {
            let cell = &row[column];
            let cell_width = symbol_width(cell.symbol());
            if cell.symbol() != " " || cell.bg != bg || cell.modifier != Modifier::empty() {
                last_nonblank_column = column + (cell_width.saturating_sub(1));
            }
            column += cell_width.max(1); // treat zero-width symbols as width 1
        }

        // A row with room left at its end is erased whole and its cells are written again. A
        // terminal remembers how far a row ever reached: one erased only from its middle keeps
        // that length, and on a narrower screen its reflow wraps the erased blanks onto a row of
        // their own, so every row of the scrollback would come back double spaced.
        let erased = last_nonblank_column + 1 < row.len();
        if erased {
            let (x, y) = a.pos_of(row_start);
            sink.link(None)?;
            sink.clear_to_end(x, y, bg)?;
        }
        let links = crate::hyperlink::buffer_row_links(b, y);

        // Cells invalidated by drawing/replacing preceding multi-width characters:
        let mut invalidated: usize = 0;
        // Cells from the current buffer to skip due to preceding multi-width characters taking
        // their place (the skipped cells should be blank anyway), or due to per-cell-skipping:
        let mut to_skip: usize = 0;
        for (column, (current, previous)) in row.iter().zip(previous_row.iter()).enumerate() {
            let changed = if erased {
                // The erase left blanks of the row's trailing background.
                current.symbol() != " " || current.bg != bg || !current.modifier.is_empty()
            } else {
                repaint_all || current != previous || invalidated > 0
            };
            if !matches!(current.diff_option, CellDiffOption::Skip)
                && changed
                && to_skip == 0
                && column <= last_nonblank_column
            {
                let (x, y) = a.pos_of(row_start + column);
                let link = links
                    .iter()
                    .find(|link| link.columns.contains(&x))
                    .map(|link| link.target.as_str());
                sink.link(link)?;
                sink.put(x, y, current)?;
                last_put = Some(Position { x, y });
            }

            let current_width = symbol_width(current.symbol());
            to_skip = current_width.saturating_sub(1);

            let affected_width = std::cmp::max(current_width, symbol_width(previous.symbol()));
            invalidated = std::cmp::max(affected_width, invalidated).saturating_sub(1);
        }
        sink.link(None)?;
    }
    Ok(last_put)
}

/// Writes diff commands as terminal escape sequences, moving the cursor only when the next cell
/// is not right after the previous one.
struct TerminalSink<'w, W: Write> {
    writer: &'w mut W,
    fg: Color,
    bg: Color,
    modifier: Modifier,
    last_pos: Option<Position>,
    /// The hyperlink open for the cells being written.
    link: Option<String>,
}

impl<'w, W: Write> TerminalSink<'w, W> {
    fn new(writer: &'w mut W) -> Self {
        Self {
            writer,
            fg: Color::Reset,
            bg: Color::Reset,
            modifier: Modifier::empty(),
            last_pos: None,
            link: None,
        }
    }

    fn move_to(&mut self, x: u16, y: u16) -> io::Result<()> {
        // Move the cursor if the previous location was not (x - 1, y)
        if !matches!(self.last_pos, Some(p) if x == p.x + 1 && y == p.y) {
            queue!(self.writer, MoveTo(x, y))?;
        }
        self.last_pos = Some(Position { x, y });
        Ok(())
    }

    /// Reset colors and attributes, as the end of a frame requires.
    fn finish(mut self) -> io::Result<()> {
        self.link(None)?;
        queue!(
            self.writer,
            SetForegroundColor(crossterm::style::Color::Reset),
            SetBackgroundColor(crossterm::style::Color::Reset),
            SetAttribute(crossterm::style::Attribute::Reset),
        )
    }
}

impl<W: Write> DiffSink for TerminalSink<'_, W> {
    fn link(&mut self, target: Option<&str>) -> io::Result<()> {
        if self.link.as_deref() == target {
            return Ok(());
        }
        queue!(self.writer, Print(crate::hyperlink::osc8(target)))?;
        self.link = target.map(str::to_owned);
        Ok(())
    }

    fn put(&mut self, x: u16, y: u16, cell: &Cell) -> io::Result<()> {
        self.move_to(x, y)?;
        if cell.modifier != self.modifier {
            let diff = ModifierDiff {
                from: self.modifier,
                to: cell.modifier,
            };
            diff.queue(self.writer)?;
            self.modifier = cell.modifier;
        }
        if cell.fg != self.fg || cell.bg != self.bg {
            queue!(
                self.writer,
                SetColors(Colors::new(
                    crossterm_color(cell.fg),
                    crossterm_color(cell.bg)
                ))
            )?;
            self.fg = cell.fg;
            self.bg = cell.bg;
        }
        queue!(self.writer, Print(cell.symbol()))
    }

    fn clear_to_end(&mut self, x: u16, y: u16, bg: Color) -> io::Result<()> {
        self.move_to(x, y)?;
        queue!(
            self.writer,
            SetAttribute(crossterm::style::Attribute::Reset)
        )?;
        self.modifier = Modifier::empty();
        queue!(self.writer, SetBackgroundColor(crossterm_color(bg)))?;
        self.bg = bg;
        queue!(
            self.writer,
            Clear(crossterm::terminal::ClearType::UntilNewLine)
        )?;
        // An erase leaves the cursor where it began, as if the cell before it had been written.
        self.last_pos = x.checked_sub(1).map(|x| Position { x, y });
        Ok(())
    }
}

/// The `ModifierDiff` struct is used to calculate the difference between two `Modifier`
/// values. This is useful when updating the terminal display, as it allows for more
/// efficient updates by only sending the necessary changes.
struct ModifierDiff {
    pub from: Modifier,
    pub to: Modifier,
}

impl ModifierDiff {
    fn queue<W: io::Write>(self, w: &mut W) -> io::Result<()> {
        use crossterm::style::Attribute as CAttribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CAttribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::SLOW_BLINK) || removed.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) {
            queue!(w, SetAttribute(CAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) {
            queue!(w, SetAttribute(CAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CAttribute::RapidBlink))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::buffer::Cell;
    use ratatui::style::Style;

    #[test]
    fn converts_ratatui_colors_for_crossterm_output() {
        assert_eq!(crossterm_color(Color::Red), CrosstermColor::DarkRed);
        assert_eq!(crossterm_color(Color::LightBlue), CrosstermColor::Blue);
        assert_eq!(crossterm_color(Color::Reset), CrosstermColor::Reset);
    }

    #[derive(Debug)]
    enum DrawCommand {
        Put { x: u16, y: u16 },
        ClearToEnd { x: u16, y: u16 },
    }

    impl DiffSink for Vec<DrawCommand> {
        fn put(&mut self, x: u16, y: u16, _cell: &Cell) -> io::Result<()> {
            self.push(DrawCommand::Put { x, y });
            Ok(())
        }

        fn clear_to_end(&mut self, x: u16, y: u16, _bg: Color) -> io::Result<()> {
            self.push(DrawCommand::ClearToEnd { x, y });
            Ok(())
        }
    }

    fn diff_commands(previous: &Buffer, next: &Buffer, repaint_all: bool) -> Vec<DrawCommand> {
        let mut commands = Vec::new();
        diff_buffers(previous, next, repaint_all, &mut commands).expect("diff");
        commands
    }

    #[test]
    fn diff_buffers_does_not_emit_clear_to_end_for_full_width_row() {
        let area = Rect::new(0, 0, 3, 2);
        let previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        next.cell_mut((2, 0))
            .expect("cell should exist")
            .set_symbol("X");

        let commands = diff_commands(&previous, &next, false);

        let clear_count = commands
            .iter()
            .filter(|command| matches!(command, DrawCommand::ClearToEnd { y, .. } if *y == 0))
            .count();
        assert_eq!(
            0, clear_count,
            "expected diff_buffers not to emit ClearToEnd; commands: {commands:?}",
        );
        assert!(
            commands
                .iter()
                .any(|command| matches!(command, DrawCommand::Put { x: 2, y: 0, .. })),
            "expected diff_buffers to update the final cell; commands: {commands:?}",
        );
    }

    #[test]
    fn a_shortened_row_is_erased_whole_and_written_again() {
        let area = Rect::new(0, 0, 10, 1);
        let mut previous = Buffer::empty(area);
        let mut next = Buffer::empty(area);

        previous.set_string(0, 0, "中文", Style::default());
        next.set_string(0, 0, "中", Style::default());

        let commands = diff_commands(&previous, &next, false);
        assert_eq!(
            format!("{commands:?}"),
            format!(
                "{:?}",
                [
                    DrawCommand::ClearToEnd { x: 0, y: 0 },
                    DrawCommand::Put { x: 0, y: 0 }
                ]
            ),
        );
    }
}

#[cfg(test)]
mod row_skip_tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use ratatui::style::Style;

    struct CountingSink {
        puts: usize,
        clears: usize,
    }

    impl DiffSink for CountingSink {
        fn put(&mut self, _x: u16, _y: u16, _cell: &Cell) -> io::Result<()> {
            self.puts += 1;
            Ok(())
        }

        fn clear_to_end(&mut self, _x: u16, _y: u16, _bg: Color) -> io::Result<()> {
            self.clears += 1;
            Ok(())
        }
    }

    fn diff(previous: &Buffer, next: &Buffer, repaint_all: bool) -> (usize, usize) {
        let mut sink = CountingSink { puts: 0, clears: 0 };
        diff_buffers(previous, next, repaint_all, &mut sink).expect("diff");
        (sink.puts, sink.clears)
    }

    #[test]
    fn identical_frames_emit_nothing() {
        let area = Rect::new(0, 0, 20, 4);
        let mut frame = Buffer::empty(area);
        frame.set_string(0, 1, "stable row", Style::default());
        assert_eq!(diff(&frame.clone(), &frame, false), (0, 0));
    }

    #[test]
    fn only_the_changed_row_is_visited() {
        let area = Rect::new(0, 0, 20, 4);
        let mut previous = Buffer::empty(area);
        previous.set_string(0, 0, "unchanged", Style::default());
        previous.set_string(0, 2, "before", Style::default());
        let mut next = previous.clone();
        next.set_string(0, 2, "after!", Style::default());

        // The changed row is erased and its six cells written again; the other rows cost nothing.
        assert_eq!(diff(&previous, &next, false), (6, 1));
    }

    #[test]
    fn repaint_all_visits_unchanged_rows_for_their_trailing_clear() {
        let area = Rect::new(0, 0, 20, 2);
        let mut frame = Buffer::empty(area);
        frame.set_string(0, 0, "x", Style::default());
        let (_, clears) = diff(&frame.clone(), &frame, true);
        assert_eq!(clears, 2);
    }

    #[test]
    fn repaint_all_erases_unknown_terminal_contents_before_writing() {
        let area = Rect::new(0, 0, 4, 1);
        let mut frame = Buffer::empty(area);
        frame.set_string(0, 0, "a b", Style::default());
        assert_eq!(diff(&frame, &frame, true), (2, 1));
    }

    #[test]
    fn a_full_width_row_writes_only_its_changed_cells() {
        let area = Rect::new(0, 0, 4, 1);
        let mut previous = Buffer::empty(area);
        previous.set_string(0, 0, "abcd", Style::default());
        let mut next = previous.clone();
        next.set_string(1, 0, "x", Style::default());
        assert_eq!(diff(&previous, &next, false), (1, 0));
    }
}
