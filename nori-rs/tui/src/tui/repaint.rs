//! Clearing the screen, and repainting it when it no longer shows what Nori drew.
//!
//! The clear-screen key pushes everything on screen into the scrollback, where it stays whole,
//! and starts a clean region: it scrolls rather than erases, because `ESC[2J` pushes the screen
//! into the scrollback on some terminals and loses it on others, and `ESC[3J` deletes the
//! scrollback on all of them (see `stage`).
//!
//! A repaint, for recovering a screen that shows something else (another program wrote to it, a
//! view that covered it closed), erases the region and draws every row of the frame again.

use super::Tui;

impl Tui {
    /// Push everything on screen into the scrollback at the next draw: the conversation scrolls
    /// up and off, and the composer starts again at the top of an empty screen.
    pub(crate) fn request_clear_screen(&mut self) {
        self.presenter.stage.request_clear();
        self.frame_requester().schedule_frame();
    }

    /// Repaint the region on the next draw.
    pub(crate) fn request_full_repaint(&mut self) {
        self.terminal.invalidate();
        self.presenter.full_repaint = true;
        self.frame_requester().schedule_frame();
    }
}
