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
    /// Toasts on screen, and whether the last frame drew them (so the next must not scroll them).
    pub(crate) toasts: toast::Toasts,
    toasts_drawn: bool,
    /// The next frame erases the region and draws every row of it.
    pub(crate) full_repaint: bool,
    /// The row the last frame left the cursor on: after a resize, how far the cursor has moved
    /// says how far the terminal moved the screen.
    parked_row: u16,
    /// The first row of the bottom block as the last frame drew it.
    block_top: u16,
}

impl Presenter {
    /// Find the conversation again after a resize.
    ///
    /// The terminal moves the screen by itself: a shorter screen pushes the top rows into the
    /// scrollback so the cursor stays on screen, a taller one may pull rows back. How the cursor
    /// row moved says by how many rows (`report` is where the cursor is now; the last frame left
    /// it on `parked_row`). Rows of the tail that were pushed are in the scrollback now, so the
    /// stage marks them committed; the rest of the region (and a pulled-back shift of it) is
    /// erased where it stands, from the first row of the region down, and the tail is drawn again
    /// at the new width from there. Rows above the region, the scrollback and the rows the
    /// terminal reflows are never touched.
    ///
    /// Without a report (the terminal did not answer) the shift is assumed to be the push a
    /// shorter screen needs, or the pull a taller one allows, and nothing is marked committed: a
    /// row may then be in the scrollback and on screen at once, or an old row stay above the
    /// redrawn tail, which is better than a screen that lost its context.
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
        let parked = i32::from(self.parked_row);
        let report = report.filter(|report| report.screen == screen);
        // Rows the screen moved up (negative: down) under the region.
        let shift = match &report {
            Some(report) => parked - i32::from(report.row),
            None if screen.height > old.bottom() => {
                i32::from(old.bottom()) - i32::from(screen.height)
            }
            None => (parked - i32::from(screen.height.saturating_sub(1))).max(0),
        };
        let top = (i32::from(old.y) - shift).clamp(0, i32::from(screen.height.saturating_sub(1)));
        if report.is_some() {
            // The shell's rows above the region went first; what is left of the push came off the
            // tail. Rows of a view (a focused prompt) cannot be matched to the conversation.
            let pushed = usize::try_from(shift - i32::from(old.y)).unwrap_or(0);
            self.stage
                .note_lost_rows(pushed.min(self.stage.displayed().len()));
        }
        let top = u16::try_from(top).unwrap_or(0);
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
        // Toasts are drawn over the first rows of the region: rows with a toast on them must be
        // typed, not scrolled, or the toast would end up in the scrollback.
        let toasts_expired = self.toasts.expire(Instant::now());
        if self.toasts_drawn || toasts_expired {
            self.stage.forget_screen();
        }

        let screen = terminal.size()?;
        let top = terminal.viewport_area.y;
        let plan = self.stage.plan(view, screen, top, height, popup_rows);
        self.stage.commit(terminal, &plan)?;

        let Self { stage, toasts, .. } = self;
        let block = plan.block();
        let caret = plan.caret();
        terminal.draw_in(block, Some(Position::new(0, block.y)), |frame| {
            stage.draw(&plan, frame.buffer);
            draw_fn(frame);
            if let Some(caret) = caret {
                frame.set_cursor_position(caret);
            }
            toast::draw(frame.buffer, toasts, 0, 3);
        })?;
        self.toasts_drawn = !self.toasts.is_empty();
        self.parked_row = terminal.last_known_cursor_pos.y;
        self.block_top = block.y;
        Ok(())
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
