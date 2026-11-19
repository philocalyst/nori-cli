//! Brief, unobtrusive messages in the top right of the screen.
//!
//! A toast is a few cells drawn into the frame over the right end of the topmost rows of the
//! region. It is drawn again by every frame while it lives, and when it ends the next frame no
//! longer draws it, which erases it. Nothing is added to the conversation.

use std::time::Duration;
use std::time::Instant;

use ratatui::buffer::Buffer;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

/// How long a toast stays.
pub(crate) const TOAST_LIFETIME: Duration = Duration::from_secs(4);

/// Toasts shown at once; a new one pushes the oldest out.
const MAX_TOASTS: usize = 3;

/// The most of a message that is shown, in columns.
const MAX_TEXT_COLUMNS: usize = 44;

/// Small marks to put in front, picked by the text so one message always gets the same one.
const SPARKLES: [&str; 5] = ["✦", "✧", "❀", "♪", "☾"];

#[derive(Debug)]
struct Toast {
    text: String,
    shown_at: Instant,
}

/// The toasts on screen.
#[derive(Debug, Default)]
pub(crate) struct Toasts {
    items: Vec<Toast>,
}

impl Toasts {
    pub(crate) fn push(&mut self, text: String, now: Instant) {
        // The same message again just starts its time over.
        self.items.retain(|toast| toast.text != text);
        self.items.push(Toast {
            text,
            shown_at: now,
        });
        if self.items.len() > MAX_TOASTS {
            self.items.remove(0);
        }
    }

    /// Drop the toasts whose time is up, and say whether any went.
    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let before = self.items.len();
        self.items
            .retain(|toast| now.duration_since(toast.shown_at) < TOAST_LIFETIME);
        self.items.len() != before
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// The words a toast shows: a sparkle, then the message cut to fit.
fn label(text: &str) -> String {
    let sparkle = SPARKLES[text.bytes().map(usize::from).sum::<usize>() % SPARKLES.len()];
    let mut shown = String::new();
    let mut columns = 0;
    for c in text.chars().filter(|c| !c.is_control()) {
        let width = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if columns + width > MAX_TEXT_COLUMNS {
            shown.push('…');
            break;
        }
        shown.push(c);
        columns += width;
    }
    format!(" {sparkle} {shown} ")
}

/// Draw every toast into `buffer`, newest last, from `first_row` of it down over up to `rows`
/// rows, flush against the right edge.
pub(crate) fn draw(buffer: &mut Buffer, toasts: &Toasts, first_row: u16, rows: u16) {
    let area = buffer.area;
    for (index, toast) in toasts
        .items
        .iter()
        .rev()
        .take(usize::from(rows))
        .enumerate()
    {
        let y = area.y + first_row + u16::try_from(index).unwrap_or(0);
        if y >= area.bottom() {
            break;
        }
        let text = label(&toast.text);
        let columns = u16::try_from(UnicodeWidthStr::width(text.as_str())).unwrap_or(area.width);
        let x = area.right().saturating_sub(columns.saturating_add(1));
        buffer.set_string(
            x,
            y,
            text,
            Style::new()
                .fg(Color::Cyan)
                .add_modifier(Modifier::REVERSED),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn a_toast_lives_for_its_lifetime_and_then_goes() {
        let start = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("hello".into(), start);
        assert!(!toasts.expire(start + Duration::from_secs(1)));
        assert!(!toasts.is_empty());
        assert!(toasts.expire(start + TOAST_LIFETIME + Duration::from_millis(1)));
        assert!(toasts.is_empty());
    }

    #[test]
    fn the_same_message_restarts_instead_of_stacking_and_only_a_few_show() {
        let start = Instant::now();
        let mut toasts = Toasts::default();
        toasts.push("same".into(), start);
        toasts.push("same".into(), start + Duration::from_secs(3));
        assert_eq!(toasts.items.len(), 1);
        assert!(
            !toasts.expire(start + Duration::from_secs(5)),
            "restarted at 3s"
        );

        for n in 0..6 {
            toasts.push(format!("t{n}"), start);
        }
        assert_eq!(toasts.items.len(), MAX_TOASTS);
        assert_eq!(toasts.items.last().map(|t| t.text.as_str()), Some("t5"));
    }

    #[test]
    fn a_long_message_is_cut_with_an_ellipsis_and_gets_a_stable_sparkle() {
        let long = "x".repeat(100);
        let text = label(&long);
        assert!(text.ends_with("… "), "{text:?}");
        assert!(UnicodeWidthStr::width(text.as_str()) <= MAX_TEXT_COLUMNS + 6);
        assert_eq!(label("title"), label("title"));
    }

    #[test]
    fn drawing_goes_to_the_right_edge() {
        let mut toasts = Toasts::default();
        toasts.push("hi".into(), Instant::now());
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 6));
        draw(&mut buffer, &toasts, 4, 2);
        let row: String = (0..80)
            .map(|x| buffer[(x, 4)].symbol().to_owned())
            .collect();
        // " ✦ hi " is six columns; flush right leaves one column of margin.
        assert_eq!(row.trim_end().chars().count(), 79);
        assert!(row.contains("hi"));
    }

    #[test]
    fn nothing_is_drawn_without_toasts_or_rows() {
        let mut buffer = Buffer::empty(ratatui::layout::Rect::new(0, 0, 80, 3));
        let blank = buffer.clone();
        draw(&mut buffer, &Toasts::default(), 0, 3);
        assert_eq!(buffer, blank);
        let mut toasts = Toasts::default();
        toasts.push("x".into(), Instant::now());
        draw(&mut buffer, &toasts, 0, 0);
        assert_eq!(buffer, blank);
    }
}
