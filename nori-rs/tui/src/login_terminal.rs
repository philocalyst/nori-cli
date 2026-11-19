//! A provider's own sign-in program, in a panel above the prompt.
//!
//! Signing in to Claude Code, Gemini, OpenCode or Antigravity runs that provider's CLI. Its output
//! is a terminal program's screen, redrawn in place, so a terminal emulator reads it and the panel
//! draws what the emulator shows. The program gets a very wide screen, so it never cuts a URL into
//! pieces; the panel shows each URL once, as a short link ("↗ Open the sign-in page") that the
//! terminal opens on a click, that `Ctrl+O` opens and `Ctrl+Y` copies.
//!
//! What the user types is Nori's own input line, not the program's: it has a caret, moves by
//! character and word, takes a paste whole, and is sent to the program in one piece on Enter.
//! While it is empty, arrows, Tab and Enter go straight to the program, for the programs that
//! ask with a menu.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use crate::render::renderable::Renderable;

/// Rows the sign-in program gets.
pub(crate) const ROWS: u16 = 14;
/// Columns the sign-in program gets: wide enough that nothing it prints is wrapped.
pub(crate) const COLS: u16 = 1000;

/// What a key did.
pub(crate) enum KeyOutcome {
    /// Send these bytes to the program.
    Send(Vec<u8>),
    /// Handled here; draw again.
    Handled,
    /// Not a key the sign-in takes.
    Ignored,
}

pub(crate) struct LoginTerminal {
    parser: vt100::Parser,
    provider: String,
    input: String,
    /// Byte offset of the caret in `input`, always on a character boundary.
    caret: usize,
    /// A one-line note under the input (the link was copied, say).
    note: Option<String>,
    /// No program runs: the panel only says what it waits for (the agent signing in by itself
    /// in the browser, a check that the sign-in took).
    waiting: Option<String>,
}

impl LoginTerminal {
    pub(crate) fn new(provider: String) -> Self {
        Self {
            parser: vt100::Parser::new(ROWS, COLS, 0),
            provider,
            input: String::new(),
            caret: 0,
            note: None,
            waiting: None,
        }
    }

    /// A panel with no program in it, saying what it waits for.
    pub(crate) fn waiting(provider: String, what: &str) -> Self {
        Self {
            waiting: Some(what.to_owned()),
            ..Self::new(provider)
        }
    }

    /// Bytes the program wrote.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// The program's rows, as text, trailing spaces trimmed.
    fn rows(&self) -> Vec<String> {
        let screen = self.parser.screen();
        (0..ROWS)
            .map(|row| {
                screen
                    .contents_between(row, 0, row, COLS)
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    /// The first URL the program printed: where it wants the user to sign in.
    pub(crate) fn link(&self) -> Option<String> {
        self.rows()
            .iter()
            .find_map(|row| crate::hyperlink::urls_in(row).into_iter().next())
    }

    /// What the program last said, oldest first.
    pub(crate) fn last_words(&self, count: usize) -> Vec<String> {
        let mut lines: Vec<String> = self
            .rows()
            .into_iter()
            .rev()
            .map(|row| row.trim().to_owned())
            .filter(|row| !row.is_empty())
            .take(count)
            .collect();
        lines.reverse();
        lines
    }

    /// Text pasted into the sign-in: it goes into the input line, whole.
    pub(crate) fn paste(&mut self, text: &str) {
        let text = text.trim_end_matches(['\r', '\n']);
        self.input.insert_str(self.caret, text);
        self.caret += text.len();
        self.note = None;
    }

    /// A key, while the sign-in has the keyboard.
    pub(crate) fn key(&mut self, key: KeyEvent) -> KeyOutcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let by_word = alt || control;
        match key.code {
            KeyCode::Char('o') if control => {
                if let Some(link) = self.link() {
                    self.note = Some(if webbrowser::open(&link).is_ok() {
                        "opened the sign-in page in your browser".to_owned()
                    } else {
                        "could not open a browser; Ctrl+Y copies the link".to_owned()
                    });
                }
                KeyOutcome::Handled
            }
            KeyCode::Char('y') if control => {
                if let Some(link) = self.link() {
                    self.note = Some(match copy(&link) {
                        Ok(()) => "copied the sign-in link".to_owned(),
                        Err(error) => format!("could not copy: {error}"),
                    });
                }
                KeyOutcome::Handled
            }
            KeyCode::Char('u') if control => {
                self.input.clear();
                self.caret = 0;
                KeyOutcome::Handled
            }
            KeyCode::Char('a') if control => {
                self.caret = 0;
                KeyOutcome::Handled
            }
            KeyCode::Char('e') if control => {
                self.caret = self.input.len();
                KeyOutcome::Handled
            }
            KeyCode::Char('b') if alt => {
                self.caret = word_left(&self.input, self.caret);
                KeyOutcome::Handled
            }
            KeyCode::Char('f') if alt => {
                self.caret = word_right(&self.input, self.caret);
                KeyOutcome::Handled
            }
            KeyCode::Char(c) if !control => {
                self.input.insert(self.caret, c);
                self.caret += c.len_utf8();
                self.note = None;
                KeyOutcome::Handled
            }
            KeyCode::Backspace if !self.input.is_empty() => {
                let start = if by_word {
                    word_left(&self.input, self.caret)
                } else {
                    previous(&self.input, self.caret)
                };
                self.input.replace_range(start..self.caret, "");
                self.caret = start;
                KeyOutcome::Handled
            }
            KeyCode::Delete if self.caret < self.input.len() => {
                let end = next(&self.input, self.caret);
                self.input.replace_range(self.caret..end, "");
                KeyOutcome::Handled
            }
            KeyCode::Left if !self.input.is_empty() => {
                self.caret = if by_word {
                    word_left(&self.input, self.caret)
                } else {
                    previous(&self.input, self.caret)
                };
                KeyOutcome::Handled
            }
            KeyCode::Right if !self.input.is_empty() => {
                self.caret = if by_word {
                    word_right(&self.input, self.caret)
                } else {
                    next(&self.input, self.caret)
                };
                KeyOutcome::Handled
            }
            KeyCode::Home if !self.input.is_empty() => {
                self.caret = 0;
                KeyOutcome::Handled
            }
            KeyCode::End if !self.input.is_empty() => {
                self.caret = self.input.len();
                KeyOutcome::Handled
            }
            KeyCode::Enter if !self.input.is_empty() => {
                // The line goes to the program as one piece, then Enter.
                let line = std::mem::take(&mut self.input);
                self.caret = 0;
                let mut bytes = self.paste_bytes(&line);
                bytes.push(b'\r');
                KeyOutcome::Send(bytes)
            }
            // An empty line leaves the keys to the program: its menus, its Enter.
            KeyCode::Enter => KeyOutcome::Send(vec![b'\r']),
            KeyCode::Tab => KeyOutcome::Send(vec![b'\t']),
            KeyCode::BackTab => KeyOutcome::Send(b"\x1b[Z".to_vec()),
            KeyCode::Up => KeyOutcome::Send(b"\x1b[A".to_vec()),
            KeyCode::Down => KeyOutcome::Send(b"\x1b[B".to_vec()),
            KeyCode::Left => KeyOutcome::Send(b"\x1b[D".to_vec()),
            KeyCode::Right => KeyOutcome::Send(b"\x1b[C".to_vec()),
            KeyCode::Backspace => KeyOutcome::Send(vec![0x7f]),
            _ => KeyOutcome::Ignored,
        }
    }

    /// `text` as the program should receive it: bracketed when it asked for pastes that way, so
    /// its input field takes it as one piece.
    fn paste_bytes(&self, text: &str) -> Vec<u8> {
        if self.parser.screen().bracketed_paste() {
            let mut bytes = b"\x1b[200~".to_vec();
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            bytes
        } else {
            text.as_bytes().to_vec()
        }
    }

    /// The program's rows to show, with the screen row each comes from: blank leading and
    /// trailing rows dropped, and each URL taken out of its row (the link row shows it).
    fn shown_rows(&self) -> Vec<(u16, String, bool)> {
        let mut rows: Vec<(u16, String, bool)> = self
            .rows()
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let urls = crate::hyperlink::urls_in(&row);
                let had_url = !urls.is_empty();
                let mut text = row;
                for url in urls {
                    text = text.replace(&url, "↗ below");
                }
                (u16::try_from(index).unwrap_or(0), text, had_url)
            })
            .collect();
        while rows.last().is_some_and(|(_, row, _)| row.trim().is_empty()) {
            rows.pop();
        }
        let first = rows
            .iter()
            .position(|(_, row, _)| !row.trim().is_empty())
            .unwrap_or(rows.len());
        rows.drain(..first);
        rows
    }
}

impl Renderable for LoginTerminal {
    fn desired_height(&self, _width: u16) -> u16 {
        // A blank row, the title, the program's rows, the link, the input, the note, the rule.
        if self.waiting.is_some() {
            return 3;
        }
        let rows = u16::try_from(self.shown_rows().len()).unwrap_or(ROWS);
        let link = u16::from(self.link().is_some());
        let input = 1;
        let note = u16::from(self.note.is_some());
        1 + 1 + rows + link + input + note + 1
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height < 4 || area.width < 16 {
            return;
        }
        let theme = crate::style::component_theme();
        let mut y = area.y + 1;
        let bottom = area.bottom();
        let text_x = area.x + 2;
        let width = area.width.saturating_sub(2);
        let rail = |buf: &mut Buffer, y: u16| {
            buf.set_span(area.x, y, &Span::styled("│", theme.faint), 1);
        };

        let state = self.waiting.as_deref().unwrap_or("signing in");
        let (glyph, glyph_style) = ("◆", theme.accent_violet);
        Line::from(vec![
            Span::styled(
                format!("{glyph} "),
                glyph_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(self.provider.clone(), theme.text_strong),
            Span::styled(format!(" · {state}"), theme.muted),
            Span::styled("   esc cancels", theme.faint),
        ])
        .render(Rect::new(area.x, y, area.width, 1), buf);
        y += 1;
        if self.waiting.is_some() {
            return;
        }

        // The program's screen, row by row, each cut at the panel's edge.
        let screen = self.parser.screen();
        for (row, text, had_url) in self.shown_rows() {
            if y >= bottom {
                return;
            }
            rail(buf, y);
            if had_url {
                // A row whose URL was taken out is drawn as text, plainly.
                Line::from(Span::styled(text, theme.text_secondary))
                    .render(Rect::new(text_x, y, width, 1), buf);
            } else {
                for col in 0..width {
                    let Some(cell) = screen.cell(row, col) else {
                        continue;
                    };
                    if cell.is_wide_continuation() {
                        continue;
                    }
                    let contents = cell.contents();
                    let symbol = if contents.is_empty() { " " } else { contents };
                    if let Some(target) = buf.cell_mut((text_x + col, y)) {
                        target.set_symbol(symbol).set_style(cell_style(cell));
                    }
                }
            }
            y += 1;
        }

        // The sign-in page, as a link.
        if let Some(link) = self.link()
            && y < bottom
        {
            rail(buf, y);
            let label = "↗ Open the sign-in page";
            Line::from(vec![
                Span::styled(label, theme.link.add_modifier(Modifier::BOLD)),
                Span::styled("   ^O opens · ^Y copies", theme.faint),
            ])
            .render(Rect::new(text_x, y, width, 1), buf);
            let label_width = u16::try_from(label.width()).unwrap_or(0);
            crate::hyperlink::place(Rect::new(text_x, y, label_width.min(width), 1), link);
            y += 1;
        }

        // The input line: Nori's own, with a caret.
        if y < bottom {
            rail(buf, y);
            let field = Rect::new(text_x, y, width, 1);
            buf.set_style(field, theme.input);
            let mut spans = vec![Span::styled(
                "❯ ",
                theme.accent_violet.add_modifier(Modifier::BOLD),
            )];
            if self.input.is_empty() {
                spans.push(Span::styled(
                    "paste or type the code here · Enter sends it",
                    theme.faint,
                ));
            } else {
                let (before, rest) = self.input.split_at(self.caret);
                let mut chars = rest.chars();
                let at = chars.next().map_or_else(|| " ".to_owned(), String::from);
                spans.push(Span::styled(before.to_owned(), theme.text_strong));
                spans.push(Span::styled(
                    at,
                    theme.text_strong.add_modifier(Modifier::REVERSED),
                ));
                spans.push(Span::styled(chars.as_str().to_owned(), theme.text_strong));
            }
            Line::from(spans).render(field, buf);
            y += 1;
        }

        if let Some(note) = &self.note
            && y < bottom
        {
            rail(buf, y);
            Line::from(Span::styled(note.clone(), theme.accent_mint))
                .render(Rect::new(text_x, y, width, 1), buf);
            y += 1;
        }

        if y < bottom {
            buf.set_span(
                area.x,
                y,
                &Span::styled(
                    format!("╰{}", "─".repeat(usize::from(width.min(48)))),
                    theme.faint,
                ),
                width.saturating_add(2),
            );
        }
    }
}

fn previous(text: &str, at: usize) -> usize {
    text[..at]
        .char_indices()
        .last()
        .map_or(0, |(index, _)| index)
}

fn next(text: &str, at: usize) -> usize {
    text[at..]
        .chars()
        .next()
        .map_or(text.len(), |c| at + c.len_utf8())
}

fn word_left(text: &str, at: usize) -> usize {
    let head = text[..at].trim_end_matches(|c: char| !c.is_alphanumeric());
    head.rfind(|c: char| !c.is_alphanumeric())
        .map_or(0, |index| index + 1)
}

fn word_right(text: &str, at: usize) -> usize {
    let rest = &text[at..];
    let skipped = rest.len()
        - rest
            .trim_start_matches(|c: char| !c.is_alphanumeric())
            .len();
    let word = rest[skipped..]
        .find(|c: char| !c.is_alphanumeric())
        .unwrap_or(rest.len() - skipped);
    at + skipped + word
}

#[cfg(not(target_os = "android"))]
fn copy(text: &str) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text.to_owned()))
        .map_err(|error| error.to_string())
}

#[cfg(target_os = "android")]
fn copy(_text: &str) -> Result<(), String> {
    Err("there is no clipboard here".to_owned())
}

fn cell_style(cell: &vt100::Cell) -> Style {
    let mut style = Style::default();
    if let Some(color) = color(cell.fgcolor()) {
        style = style.fg(color);
    }
    if let Some(color) = color(cell.bgcolor()) {
        style = style.bg(color);
    }
    for (on, modifier) in [
        (cell.bold(), Modifier::BOLD),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
        (cell.dim(), Modifier::DIM),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    style
}

#[allow(clippy::disallowed_methods)]
fn color(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Color::Indexed(index)),
        vt100::Color::Rgb(red, green, blue) => Some(Color::Rgb(red, green, blue)),
    }
}
