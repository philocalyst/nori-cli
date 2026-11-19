//! The prompt bar while the agent works.
//!
//! There is no status row above the prompt, no spinner and no stand-in text: the bar the user
//! types into says the agent is busy with a sweep gliding along its top edge, and how long the
//! turn has run sits at the far right of the prompt row. Nothing about it takes a row of its own,
//! so starting and finishing a turn never moves the screen.

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Span;

use super::chat_composer::ChatComposer;
use crate::render::renderable::Renderable;
use crate::status_indicator_widget::StatusIndicatorWidget;

/// Where the composer lets the activity show.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ActivitySlots {
    /// The prompt's first row, and how many of its columns the user's text takes.
    pub(crate) prompt_row: Option<(Rect, u16)>,
    /// The blank row along the top edge of the prompt.
    pub(crate) rail: Option<Rect>,
}

/// The composer, with the agent's activity drawn into it.
pub(crate) struct ComposerWithActivity<'a> {
    pub(crate) composer: &'a ChatComposer,
    pub(crate) activity: Option<&'a StatusIndicatorWidget>,
}

impl Renderable for ComposerWithActivity<'_> {
    fn desired_height(&self, width: u16) -> u16 {
        self.composer.desired_height(width)
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.composer.cursor_pos(area)
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.composer.render(area, buf);
        if let Some(activity) = self.activity
            && let Some(slots) = self.composer.activity_slots(area)
        {
            paint(activity, slots, buf);
        }
    }
}

/// How long one pass of the sweep takes.
const SWEEP: Duration = Duration::from_millis(1800);

fn paint(activity: &StatusIndicatorWidget, slots: ActivitySlots, buf: &mut Buffer) {
    let now = Instant::now();
    let animated = activity.animations_enabled();
    if animated {
        activity.request_frame_in(Duration::from_millis(40));
    }
    let theme = crate::style::component_theme();

    if let Some(rail) = slots.rail
        && animated
    {
        sweep(
            buf,
            rail,
            now.saturating_duration_since(activity.started_at()),
            &theme,
        );
    }

    // The time, at the far right of the prompt row, once there is some ("0s" says nothing), and
    // never over what the user typed.
    let seconds = activity.elapsed_seconds();
    if let Some((row, typed)) = slots.prompt_row
        && seconds > 0
    {
        let label = crate::status_indicator_widget::fmt_elapsed_compact(seconds);
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        if row.width > typed.saturating_add(width).saturating_add(3) {
            buf.set_span(
                row.right() - width - 1,
                row.y,
                &Span::styled(label, theme.muted),
                width,
            );
        }
    }
}

/// A short bright segment gliding along `rail`, fading at its ends.
fn sweep(buf: &mut Buffer, rail: Rect, elapsed: Duration, theme: &nori_tui_components::Theme) {
    let width = i32::from(rail.width);
    if width < 8 {
        return;
    }
    let length = (width / 6).clamp(6, 28);
    let period = SWEEP.as_millis().max(1);
    let phase = f64::from(u32::try_from(elapsed.as_millis() % period).unwrap_or(0))
        / f64::from(u32::try_from(period).unwrap_or(1));
    // The segment enters at the left edge and leaves past the right one.
    let head = (phase * f64::from(width + length)) as i32 - length;
    for offset in 0..length {
        let x = head + offset;
        if !(0..width).contains(&x) {
            continue;
        }
        // Brightest in the middle of the segment.
        let center = f64::from(length - 1) / 2.0;
        let distance = (f64::from(offset) - center).abs() / center.max(1.0);
        let style: Style = if distance < 0.35 {
            theme.accent_mint.bold()
        } else if distance < 0.75 {
            theme.accent_mint
        } else {
            theme.faint
        };
        let column = rail.x + u16::try_from(x).unwrap_or(0);
        buf.set_span(column, rail.y, &Span::styled("─", style), 1);
    }
}
