//! Presenting a frame on a terminal: the stage's plan, the cell grid, the toasts and the cursor.
//!
//! Everything that decides what reaches the screen lives here, generic over the terminal's
//! backend, so the real terminal and the in-memory one of the tests (`stage/sim.rs`) run the same
//! code. [`super::Tui`] adds what only a real terminal has: synchronized-update markers, one
//! `write` per frame, raw mode and suspend.

use std::io;
use std::io::Write;
use std::time::Instant;

use crossterm::queue;
use ratatui::backend::Backend;
use ratatui::layout::Position;
use ratatui::layout::Size;

use super::toast;
use crate::custom_terminal::Frame;
use crate::custom_terminal::Terminal;
use crate::stage::Stage;
use crate::stage::StageView;

/// Where the terminal says the cursor is after a resize, and the size of the screen it said it for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CursorReport {
    pub(crate) screen: Size,
    pub(crate) row: u16,
}

impl CursorReport {
    /// Ask the terminal. A reply cannot be read while the event reader is waiting for input, so
    /// this is called from the event stream right after it was handed a resize, when nothing is
    /// waiting. Gives up after a couple of seconds.
    pub(crate) fn query() -> Option<Self> {
        let (_, row) = crossterm::cursor::position().ok()?;
        let (cols, rows) = crossterm::terminal::size().ok()?;
        Some(Self {
            screen: Size::new(cols, rows),
            row,
        })
    }
}

/// What the event stream tells the next frame about a resize.
#[derive(Default)]
pub(crate) struct ResizeSignal {
    pending: std::sync::atomic::AtomicBool,
    report: std::sync::Mutex<Option<CursorReport>>,
}

impl ResizeSignal {
    /// A resize happened, and the cursor is `report` after it.
    pub(crate) fn note(&self, report: Option<CursorReport>) {
        if let Ok(mut slot) = self.report.lock() {
            *slot = report;
        }
        self.pending
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether a resize happened since the last frame, and where the cursor was then.
    pub(crate) fn take(&self) -> (bool, Option<CursorReport>) {
        let pending = self
            .pending
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        let report = self.report.lock().ok().and_then(|mut slot| slot.take());
        (pending, report)
    }
}

#[derive(Default)]
pub(crate) struct Presenter {
    /// What the screen shows of the conversation, and what has left it for the scrollback.
    pub(crate) stage: Stage,
    /// Toasts, drawn on the status line: never over the conversation, which is the terminal's.
    pub(crate) toasts: toast::Toasts,
    /// When the toasts on screen must be drawn again (they slide), if any are.
    pub(super) toast_frame: Option<std::time::Duration>,
    /// The next frame erases the region and draws every row of it.
    pub(crate) full_repaint: bool,
    /// The row the last frame left the cursor on: after a resize, how far the cursor has moved
    /// says how far the terminal moved the screen.
    pub(super) parked_row: u16,
    /// The first row of the bottom block as the last frame drew it.
    pub(super) block_top: u16,
    /// What the terminal does to its rows when its width changes.
    pub(crate) width_change: super::reflow::WidthChange,
}

impl Presenter {
    /// Find the conversation again after a resize.
    ///
    /// The terminal moves the screen by itself: a shorter screen pushes the top rows into the
    /// scrollback so the cursor stays on screen, a taller one may pull rows back, and a new width
    /// re-wraps every row, the region's included (one row of the tail may become three). Where
    /// the cursor is now (`report`) and how many rows lie between the region's first row and the
    /// cursor once re-wrapped ([`super::reflow`], from the cells of the last frame) say exactly
    /// where the region's first row went. Everything from there down is erased and drawn again at
    /// the new width, so no re-wrapped copy of the tail is left behind. Rows of the tail the
    /// terminal pushed past the top are in the scrollback now, so the stage marks them committed.
    /// Rows above the region and the scrollback are never touched.
    ///
    /// Without a report (the terminal did not answer) a region that began on the first row still
    /// does; otherwise the shift is assumed to be the push a shorter screen needs, or the pull a
    /// taller one allows, less the rows the region grew by.
    ///
    /// A bottom block that sat on the last row stays there (the stage keeps it anchored): rows a
    /// taller screen adds are left empty right above it.
    ///
    /// The printed rows above the region were re-wrapped too, a glyph at a time. Every cell of
    /// them still wholly on screen is taken back and laid out again at the new width (words
    /// wrapped, the full width used), so after a resize the screen reads as if it had been drawn
    /// for its size, and those rows can be reclaimed by a view again.
    pub(crate) fn recover_from_resize<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        screen: Size,
        report: Option<CursorReport>,
    ) where
        B: Backend<Error = io::Error> + Write,
    {
        let old = terminal.viewport_area;
        if old.is_empty() {
            return;
        }
        let old_screen = terminal.last_known_screen_size;
        let parked = i32::from(self.parked_row);
        let report = report.filter(|report| report.screen == screen);
        let cursor = Position::new(terminal.last_known_cursor_pos.x, self.parked_row);
        let rewrapped = super::reflow::Rewrapped::of(
            terminal.last_frame(),
            cursor,
            screen.width,
            self.width_change,
        );
        let above = i32::try_from(rewrapped.above_cursor()).unwrap_or(i32::MAX);
        let top = match &report {
            Some(report) => i32::from(report.row) - above,
            None if old.y == 0 => 0,
            None => {
                // Rows the screen moved up (negative: down) under the region, as a guess.
                let shift = if screen.height > old.bottom() {
                    i32::from(old.bottom()) - i32::from(screen.height)
                } else {
                    (parked - i32::from(screen.height.saturating_sub(1))).max(0)
                };
                let grown = above - (parked - i32::from(old.y));
                i32::from(old.y) - shift - grown.max(0)
            }
        };
        // The printed rows above the region: with the cursor's answer, how the terminal re-wrapped
        // them says which cells are still wholly on screen, and those are taken back to be laid
        // out again at the new width; without it, or when the region itself was pushed past the
        // top, they are the terminal's alone.
        // Taller, as wide, and the cursor stayed on its row: the terminal pulled nothing back
        // (Terminal.app does not), so the rows it added are empty ones at the bottom.
        if let Some(report) = &report
            && screen.width == old_screen.width
            && screen.height > old_screen.height
            && i32::from(report.row) == parked
        {
            self.stage.unanchor_after_bare_grow();
        }
        let top = if report.is_some() && top >= 0 {
            let taken = self.stage.reown_after_resize(
                u16::try_from(top).unwrap_or(u16::MAX),
                screen.width,
                self.width_change,
            );
            tracing::debug!(top, taken, "took back the cells still on screen");
            top - i32::from(taken)
        } else {
            self.stage.forget_printed();
            top
        };
        if report.is_some() && top < 0 {
            // The first rows of the region went past the top into the scrollback. Rows of the
            // tail that went whole are committed there; one cut in two is drawn again whole.
            // Rows of a view (a focused prompt) cannot be matched to the conversation.
            let pushed = usize::try_from(-top).unwrap_or(0);
            let mut covered = 0;
            let gone = rewrapped
                .rows
                .iter()
                .take_while(|rows| {
                    covered += **rows;
                    covered <= pushed
                })
                .count();
            self.stage
                .note_lost_rows(gone.min(self.stage.displayed().len()));
        }
        tracing::debug!(
            ?screen,
            ?old,
            ?report,
            parked,
            above,
            top,
            displayed = self.stage.displayed().len(),
            "recovering the region after a resize"
        );
        let top =
            u16::try_from(top.clamp(0, i32::from(screen.height.saturating_sub(1)))).unwrap_or(0);
        let _ = crate::stage::write::erase_rows(terminal.backend_mut(), top, screen.height);
        self.stage.forget_screen();
        terminal.set_viewport_area(ratatui::layout::Rect::new(
            0,
            top,
            screen.width,
            screen.height - top,
        ));
        terminal.invalidate();
    }

    /// Call before every frame: after a resize (the terminal told us, or its size is not what the
    /// last frame was drawn for) find the screen again.
    pub(crate) fn note_screen<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        resize_event: bool,
        report: Option<CursorReport>,
    ) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        let screen = terminal.size()?;
        if resize_event || screen != terminal.last_known_screen_size {
            self.recover_from_resize(terminal, screen, report);
        }
        Ok(())
    }

    /// Draw one frame: plan it, scroll or type what leaves the region, then draw the tail, the
    /// bottom block (`draw_fn`) and the toasts into the cell grid and write the difference.
    pub(crate) fn present<B>(
        &mut self,
        terminal: &mut Terminal<B>,
        view: &StageView<'_>,
        height: u16,
        popup_rows: u16,
        draw_fn: impl FnOnce(&mut Frame),
    ) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        if std::mem::take(&mut self.full_repaint) {
            terminal.clear()?;
            self.stage.forget_screen();
        }
        let now = Instant::now();
        self.toasts.expire(now);

        let screen = terminal.size()?;
        let top = terminal.viewport_area.y;
        let plan = self.stage.plan(view, screen, top, height, popup_rows);
        self.stage.commit(terminal, &plan)?;
        // The frame is drawn for this screen; one that changed meanwhile is found again next.
        terminal.resize(screen)?;

        let Self { stage, toasts, .. } = self;
        let block = plan.block();
        let mut toast_frame = None;
        terminal.draw_in(block, Some(Position::new(0, block.y)), |frame| {
            stage.draw(&plan, frame.buffer);
            draw_fn(frame);
            // A prompt edited where it is in the conversation has the cursor there.
            if let Some((x, y)) = plan.focus_cursor(frame.buffer.area) {
                frame.set_cursor_position(Position::new(x, y));
            }
            // The newest toast, at the right end of the block's last row (the status line).
            let status = block
                .bottom()
                .saturating_sub(1)
                .saturating_sub(frame.buffer.area.y);
            toast_frame = toast::draw(frame.buffer, toasts, status, 1, now);
        })?;
        self.toast_frame = toast_frame;
        // The rows of settled cells the frame drew are the terminal's from now on: the region
        // starts below them, and they are never drawn again.
        let printed = u16::try_from(self.stage.settle(&plan)).unwrap_or(0);
        if printed > 0 {
            let area = terminal.viewport_area;
            let rest = ratatui::layout::Rect::new(
                area.x,
                area.y + printed,
                area.width,
                area.height.saturating_sub(printed),
            );
            terminal.scroll_viewport(rest, printed);
        }
        self.parked_row = terminal.last_known_cursor_pos.y;
        self.block_top = block.y;
        Ok(())
    }

    /// When the last frame's toasts must be drawn again, if it drew any.
    pub(crate) fn toast_frame(&self) -> Option<std::time::Duration> {
        self.toast_frame
    }

    /// Where the bottom block began in the last frame.
    pub(crate) fn block_top(&self) -> u16 {
        self.block_top
    }

    /// Leave the screen for the shell: the bottom block is erased and the cursor goes to where it
    /// began, under the last row of the conversation. The conversation stays where it is.
    pub(crate) fn finish<B>(&self, terminal: &mut Terminal<B>) -> io::Result<()>
    where
        B: Backend<Error = io::Error> + Write,
    {
        if terminal.viewport_area.is_empty() {
            return Ok(());
        }
        let height = terminal.size()?.height;
        crate::stage::write::erase_rows(terminal.backend_mut(), self.block_top, height)?;
        queue!(
            terminal.backend_mut(),
            crossterm::cursor::MoveTo(0, self.block_top),
            crossterm::cursor::Show,
        )
    }
}
