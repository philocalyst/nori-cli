//! Writing rows straight to the terminal, for the two things the cell grid cannot do: pushing
//! rows into the scrollback, and typing rows the screen never showed.

use std::io;
use std::io::Write;

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Color as CColor;
use crossterm::style::Print;
use crossterm::style::SetAttribute;
use crossterm::style::SetBackgroundColor;
use crossterm::style::SetForegroundColor;
use crossterm::terminal::Clear;
use crossterm::terminal::ClearType;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::custom_terminal::crossterm_color;

/// Scroll the whole screen up by `rows`: the rows at the top go to the scrollback. A line feed on
/// the last row is the one way every terminal scrolls its history.
pub(crate) fn scroll_screen_up(
    writer: &mut impl Write,
    screen_height: u16,
    rows: u16,
) -> io::Result<()> {
    if rows == 0 || screen_height == 0 {
        return Ok(());
    }
    queue!(writer, MoveTo(0, screen_height - 1))?;
    for _ in 0..rows {
        queue!(writer, Print("\n"))?;
    }
    Ok(())
}

/// Hand `rows` to the scrollback, in order and once each, by scrolling the screen: `lift` rows
/// above them leave first as they are, then one line feed per row. The row that leaves is the one
/// at the top of the screen, so a row the top does not show (`shown(index)` is false: it is
/// clipped, covered, or changed since it was drawn) is written there just before its line feed.
/// Every row that stays on screen is moved by the terminal and never drawn again, so what the
/// person is reading, or has selected, keeps its place.
pub(crate) fn scroll_rows_off(
    writer: &mut impl Write,
    screen_height: u16,
    width: u16,
    lift: u16,
    rows: &[Line<'_>],
    shown: impl Fn(usize) -> bool,
) -> io::Result<()> {
    scroll_screen_up(writer, screen_height, lift)?;
    if rows.is_empty() || screen_height == 0 {
        return Ok(());
    }
    let texts: Vec<_> = rows.iter().map(|row| row_text(row, width)).collect();
    let mut at_bottom = lift > 0;
    for (index, row) in rows.iter().enumerate() {
        if !shown(index) {
            queue!(writer, MoveTo(0, 0))?;
            write_row(writer, row, &crate::hyperlink::line_links(&texts, index))?;
            at_bottom = false;
        }
        if !at_bottom {
            queue!(writer, MoveTo(0, screen_height - 1))?;
            at_bottom = true;
        }
        queue!(writer, Print("\n"))?;
    }
    Ok(())
}

/// Erase the rows `from..to` one at a time. Erasing "from the cursor down" at the top of the
/// screen is read by some terminals (tmux) as clearing the screen, and they push everything on it
/// into the scrollback first.
pub(crate) fn erase_rows(writer: &mut impl Write, from: u16, to: u16) -> io::Result<()> {
    for row in from..to {
        queue!(writer, MoveTo(0, row), Clear(ClearType::CurrentLine))?;
    }
    Ok(())
}

/// Type `rows` from row `first_row`, one under another, the way a program prints: past the last
/// row of the screen every further row scrolls the screen, so the rows that do not fit end in the
/// scrollback, each once and in order. Their URLs and paths are typed as hyperlinks, so they stay
/// clickable in the scrollback.
pub(crate) fn type_rows(
    writer: &mut impl Write,
    rows: &[Line<'_>],
    first_row: u16,
    width: u16,
) -> io::Result<()> {
    let texts: Vec<_> = rows.iter().map(|row| row_text(row, width)).collect();
    queue!(writer, MoveTo(0, first_row))?;
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            queue!(writer, Print("\r\n"))?;
        }
        write_row(writer, row, &crate::hyperlink::line_links(&texts, index))?;
    }
    Ok(())
}

/// `line` as the hyperlink scanner reads it.
fn row_text(line: &Line<'_>, width: u16) -> crate::hyperlink::RowText {
    let mut text = crate::hyperlink::RowText::new(width);
    let mut column = 0u16;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            text.push(column, grapheme);
            column = column.saturating_add(u16::try_from(grapheme.width()).unwrap_or(1));
        }
    }
    text
}

/// One row at the cursor: its background to the end of the row, then its spans, with the link
/// each column belongs to.
fn write_row(
    writer: &mut impl Write,
    line: &Line<'_>,
    links: &[crate::hyperlink::Link],
) -> io::Result<()> {
    queue!(
        writer,
        SetForegroundColor(crossterm_color(line.style.fg.unwrap_or(Color::Reset))),
        SetBackgroundColor(crossterm_color(line.style.bg.unwrap_or(Color::Reset))),
        Clear(ClearType::UntilNewLine),
    )?;
    let mut fg = Color::Reset;
    let mut bg = Color::Reset;
    let mut modifier = Modifier::empty();
    let mut column = 0u16;
    let mut open: Option<&str> = None;
    for span in &line.spans {
        let style = span.style.patch(line.style);
        let mut next = Modifier::empty();
        next.insert(style.add_modifier);
        next.remove(style.sub_modifier);
        if next != modifier {
            ModifierDiff {
                from: modifier,
                to: next,
            }
            .queue(writer)?;
            modifier = next;
        }
        let next_fg = style.fg.unwrap_or(Color::Reset);
        let next_bg = style.bg.unwrap_or(Color::Reset);
        if next_fg != fg || next_bg != bg {
            queue!(
                writer,
                SetForegroundColor(crossterm_color(next_fg)),
                SetBackgroundColor(crossterm_color(next_bg))
            )?;
            fg = next_fg;
            bg = next_bg;
        }
        if links.is_empty() {
            queue!(writer, Print(span.content.as_ref()))?;
            continue;
        }
        let mut run = String::new();
        for grapheme in span.content.graphemes(true) {
            let link = links
                .iter()
                .find(|link| link.columns.contains(&column))
                .map(|link| link.target.as_str());
            if link != open {
                if !run.is_empty() {
                    queue!(writer, Print(std::mem::take(&mut run)))?;
                }
                queue!(writer, Print(crate::hyperlink::osc8(link)))?;
                open = link;
            }
            run.push_str(grapheme);
            column = column.saturating_add(u16::try_from(grapheme.width()).unwrap_or(1));
        }
        if !run.is_empty() {
            queue!(writer, Print(run))?;
        }
    }
    if open.is_some() {
        queue!(writer, Print(crate::hyperlink::osc8(None)))?;
    }
    queue!(
        writer,
        SetForegroundColor(CColor::Reset),
        SetBackgroundColor(CColor::Reset),
        SetAttribute(crossterm::style::Attribute::Reset),
    )
}

/// The attribute changes that turn one set of modifiers into another.
struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue(self, writer: &mut impl Write) -> io::Result<()> {
        use crossterm::style::Attribute;
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(writer, SetAttribute(Attribute::NoReverse))?;
        }
        if removed.contains(Modifier::BOLD) {
            queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(writer, SetAttribute(Attribute::Dim))?;
            }
        }
        if removed.contains(Modifier::ITALIC) {
            queue!(writer, SetAttribute(Attribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(writer, SetAttribute(Attribute::NoUnderline))?;
        }
        if removed.contains(Modifier::DIM) {
            queue!(writer, SetAttribute(Attribute::NormalIntensity))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(writer, SetAttribute(Attribute::NotCrossedOut))?;
        }
        if removed.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
            queue!(writer, SetAttribute(Attribute::NoBlink))?;
        }
        let added = self.to - self.from;
        for (flag, attribute) in [
            (Modifier::REVERSED, Attribute::Reverse),
            (Modifier::BOLD, Attribute::Bold),
            (Modifier::ITALIC, Attribute::Italic),
            (Modifier::UNDERLINED, Attribute::Underlined),
            (Modifier::DIM, Attribute::Dim),
            (Modifier::CROSSED_OUT, Attribute::CrossedOut),
            (Modifier::SLOW_BLINK, Attribute::SlowBlink),
            (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        ] {
            if added.contains(flag) {
                queue!(writer, SetAttribute(attribute))?;
            }
        }
        Ok(())
    }
}
