//! The Helix frame: geometry of the two panes and the box drawn around them.
//!
//! ```text
//! ╭─ ◇ Sessions ───────────────┬───────────────────────────────────╮
//! │ ❯ query█          6 / 182  │ preview title             Hit 2/4 │
//! ├────────────────────────────┼───────────────────────────────────┤
//! │ ▸ list rows                │ preview rows                      │
//! ╰─ status ───────────── 3ms ─┴───────────────────────────────────╯
//! ```
//!
//! The list takes 38% of the width and the preview the rest; below [`PREVIEW_MIN_WIDTH`]
//! columns (or when hidden) the list takes the whole box and the `┬ ┼ ┴` joints disappear.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use unicode_width::UnicodeWidthStr;

pub const PREVIEW_MIN_WIDTH: u16 = 78;
pub const LIST_SHARE_PERCENT: u16 = 38;
/// Columns a pane keeps clear of the border it sits against.
pub const PANE_PAD: u16 = 1;
/// The smallest box that can show a prompt and one row.
pub const MIN_HEIGHT: u16 = 5;
pub const MIN_WIDTH: u16 = 12;

/// Where everything goes, computed from the box's outer area alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameLayout {
    pub outer: Rect,
    /// The prompt row of the list pane.
    pub prompt: Rect,
    pub list: Rect,
    /// The row beside the prompt, holding the preview's title.
    pub preview_title: Option<Rect>,
    pub preview: Option<Rect>,
    /// The column of the `┬ ┼ ┴` joints, when the preview shows.
    pub split_x: Option<u16>,
}

impl FrameLayout {
    pub fn new(outer: Rect, show_preview: bool) -> Self {
        let inner_x = outer.x + 1;
        let inner_width = outer.width.saturating_sub(2);
        let body_y = outer.y + 3;
        let body_height = outer.height.saturating_sub(4);
        let prompt_y = outer.y + 1;
        if !show_preview || outer.width < PREVIEW_MIN_WIDTH {
            return Self {
                outer,
                prompt: Rect::new(inner_x, prompt_y, inner_width, 1),
                list: Rect::new(inner_x, body_y, inner_width, body_height),
                preview_title: None,
                preview: None,
                split_x: None,
            };
        }
        // One column of the interior is the divider.
        let usable = inner_width.saturating_sub(1);
        let list_width = (usable * LIST_SHARE_PERCENT / 100).max(16).min(usable);
        let split_x = inner_x + list_width;
        let right_x = split_x + 1;
        let right_width = (outer.right() - 1).saturating_sub(right_x);
        Self {
            outer,
            prompt: Rect::new(inner_x, prompt_y, list_width, 1),
            list: Rect::new(inner_x, body_y, list_width, body_height),
            preview_title: Some(Rect::new(right_x, prompt_y, right_width, 1)),
            preview: Some(Rect::new(right_x, body_y, right_width, body_height)),
            split_x: Some(split_x),
        }
    }

    pub fn has_preview(&self) -> bool {
        self.preview.is_some()
    }
}

/// Draw the box: borders, the title on the top edge, the rule under the prompt, the joints,
/// and `status` / `trailing` on the bottom edge.
pub fn draw_frame(
    layout: &FrameLayout,
    buf: &mut Buffer,
    border: Style,
    title: (&str, Style),
    status: &str,
    trailing: &str,
) {
    let outer = layout.outer;
    if outer.width < MIN_WIDTH || outer.height < MIN_HEIGHT {
        return;
    }
    let left = outer.x;
    let right = outer.right() - 1;
    let top = outer.y;
    let bottom = outer.bottom() - 1;
    let joint = layout.split_x;

    let horizontal = |buf: &mut Buffer, y: u16, ends: (&str, &str), middle: &str| {
        for x in left..=right {
            let symbol = if x == left {
                ends.0
            } else if x == right {
                ends.1
            } else if Some(x) == joint {
                middle
            } else {
                "─"
            };
            buf[(x, y)].set_symbol(symbol).set_style(border);
        }
    };
    horizontal(buf, top, ("╭", "╮"), "┬");
    horizontal(buf, top + 2, ("├", "┤"), "┼");
    horizontal(buf, bottom, ("╰", "╯"), "┴");
    for y in top + 1..bottom {
        if y == top + 2 {
            continue;
        }
        buf[(left, y)].set_symbol("│").set_style(border);
        buf[(right, y)].set_symbol("│").set_style(border);
        if let Some(x) = joint {
            buf[(x, y)].set_symbol("│").set_style(border);
        }
    }

    let room = |from: u16, to: u16| usize::from(to.saturating_sub(from));
    let limit = joint.unwrap_or(right);
    put(
        buf,
        left + 2,
        top,
        &format!(" ◇ {} ", title.0),
        title.1,
        room(left + 2, limit),
    );
    if !status.is_empty() {
        put(
            buf,
            left + 2,
            bottom,
            &format!(" {status} "),
            border,
            room(left + 2, limit),
        );
    }
    if !trailing.is_empty() {
        let text = format!(" {trailing} ");
        let width = u16::try_from(UnicodeWidthStr::width(text.as_str())).unwrap_or(0);
        let x = right.saturating_sub(width + 1);
        if x > left + 2 {
            put(buf, x, bottom, &text, border, room(x, right));
        }
    }
}

/// Write `text` at `(x, y)` in `style`, cut to `max` columns.
fn put(buf: &mut Buffer, x: u16, y: u16, text: &str, style: Style, max: usize) {
    if max == 0 {
        return;
    }
    buf.set_stringn(x, y, text, max, style);
}
