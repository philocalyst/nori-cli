//! Fenced code blocks, streamed one line at a time.
//!
//! A fenced block is the one kind of markdown block that routinely grows to thousands of lines in
//! a single message, and highlighting is the most expensive thing the renderer does. While a
//! top-level fence is the open block, each complete line is highlighted once, with the parser
//! state carried to the next line, and committed at once: the block costs its length in total,
//! not its length on every frame.
//!
//! Lines come out exactly as rendering the finished message would produce them: the block's
//! leading separator comes from the renderer itself, and each body line has the shape the
//! renderer gives code lines (an empty indent span, then the line's spans, unwrapped).

use ratatui::text::Line;
use ratatui::text::Span;

use crate::render::highlight::LineHighlighter;

/// The opening line of a fenced code block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FenceOpener {
    marker: u8,
    len: usize,
    indent: usize,
    /// The first token of the info string, as the renderer reads it.
    lang: Option<String>,
}

impl FenceOpener {
    /// Read `line` (without its newline) as a fence opener: up to three spaces, three or more
    /// backticks or tildes, and an info string, which for backticks may not contain one.
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if indent > 3 {
            return None;
        }
        let rest = &line[indent..];
        let marker = *rest.as_bytes().first()?;
        if marker != b'`' && marker != b'~' {
            return None;
        }
        let len = rest.bytes().take_while(|byte| *byte == marker).count();
        if len < 3 {
            return None;
        }
        let info = rest[len..].trim();
        if marker == b'`' && info.contains('`') {
            return None;
        }
        let lang = info
            .split([',', ' ', '\t'])
            .find(|token| !token.is_empty())
            .map(str::to_owned);
        Some(Self {
            marker,
            len,
            indent,
            lang,
        })
    }

    /// Whether `line` (without its newline) closes this fence.
    pub(crate) fn is_closed_by(&self, line: &str) -> bool {
        let indent = line.bytes().take_while(|byte| *byte == b' ').count();
        if indent > 3 {
            return false;
        }
        let rest = &line[indent..];
        let run = rest.bytes().take_while(|byte| *byte == self.marker).count();
        run >= self.len
            && rest[run..]
                .bytes()
                .all(|byte| byte == b' ' || byte == b'\t')
    }

    /// A body line with the opener's indentation removed, as CommonMark does.
    fn body<'a>(&self, line: &'a str) -> &'a str {
        let strip = line
            .bytes()
            .take(self.indent)
            .take_while(|byte| *byte == b' ')
            .count();
        &line[strip..]
    }
}

/// How body lines are drawn.
enum Lines {
    /// No info string: lines as they are, blank ones included.
    Bare,
    /// An info string the highlighter does not know: plain lines, trailing blank lines dropped.
    Plain,
    /// A known language: highlighted lines, trailing blank lines dropped.
    Highlighted(Box<LineHighlighter>),
    /// Shell: lines painted by the shell highlighter every command in the transcript uses.
    Shell,
}

/// A fenced code block that is the open block of a streaming message.
pub(super) struct OpenFence {
    opener: FenceOpener,
    lines: Lines,
    /// Offset into the message of the first line not yet consumed.
    next_line: usize,
    /// Blank lines withheld because a highlighted block drops the blank lines it ends with; they
    /// are committed once a line with content follows.
    held_blank: usize,
    /// Bytes highlighted so far, for tests that count work.
    pub(super) highlighted_bytes: usize,
}

/// What [`OpenFence::advance`] found.
pub(super) enum FenceProgress {
    Open,
    /// It consumed as many lines as one call may, and complete lines are still waiting.
    Paused,
    /// The closing fence ends at this offset into the message.
    Closed {
        end: usize,
    },
}

/// The most lines one [`OpenFence::advance`] consumes. Highlighting is what streaming code costs,
/// so a burst of thousands of lines is spread over consecutive frames: its first lines are on
/// screen at once instead of after every one of them was highlighted.
pub(super) const MAX_LINES_PER_ADVANCE: usize = 400;

impl OpenFence {
    /// A fence whose opener ends (after its newline) at `body_start`.
    pub(super) fn new(opener: FenceOpener, body_start: usize) -> Self {
        let lines = match &opener.lang {
            None => Lines::Bare,
            Some(lang) if crate::render::highlight::is_shell_language(lang) => Lines::Shell,
            Some(lang) => LineHighlighter::for_lang(lang).map_or(Lines::Plain, |highlighter| {
                Lines::Highlighted(Box::new(highlighter))
            }),
        };
        Self {
            opener,
            lines,
            next_line: body_start,
            held_blank: 0,
            highlighted_bytes: 0,
        }
    }

    /// Consume the complete lines of `message` not consumed yet, up to
    /// [`MAX_LINES_PER_ADVANCE`], appending the lines they render to `out`. Costs the length of
    /// the lines consumed.
    pub(super) fn advance(&mut self, message: &str, out: &mut Vec<Line<'static>>) -> FenceProgress {
        let mut consumed = 0;
        while let Some(newline) = memchr::memchr(b'\n', &message.as_bytes()[self.next_line..]) {
            if consumed == MAX_LINES_PER_ADVANCE {
                return FenceProgress::Paused;
            }
            consumed += 1;
            let start = self.next_line;
            let end = start + newline + 1;
            self.next_line = end;
            let line = &message[start..end - 1];
            if self.opener.is_closed_by(line) {
                return FenceProgress::Closed { end };
            }
            self.push_line(self.opener.body(line), out);
        }
        FenceProgress::Open
    }

    /// The message ended inside the block: the line being typed is its last line (or its closing
    /// fence), and the blank lines it ends with are dropped like those of a closed block.
    pub(super) fn finish(&mut self, message: &str, out: &mut Vec<Line<'static>>) {
        let partial = &message[self.next_line..];
        if !partial.is_empty() && !self.opener.is_closed_by(partial) {
            self.push_line(self.opener.body(partial), out);
        }
        self.next_line = message.len();
    }

    fn push_line(&mut self, body: &str, out: &mut Vec<Line<'static>>) {
        let spans = match &mut self.lines {
            Lines::Bare => vec![Span::raw(body.to_owned())],
            Lines::Plain => {
                if body.is_empty() {
                    self.held_blank += 1;
                    return;
                }
                vec![Span::raw(body.to_owned())]
            }
            Lines::Shell => {
                if body.is_empty() {
                    self.held_blank += 1;
                    return;
                }
                self.highlighted_bytes += body.len() + 1;
                crate::render::bash_highlight::highlight_bash_spans(body)
            }
            Lines::Highlighted(highlighter) => {
                if body.is_empty() {
                    self.held_blank += 1;
                    return;
                }
                self.highlighted_bytes += body.len() + 1;
                let mut with_newline = String::with_capacity(body.len() + 1);
                with_newline.push_str(body);
                with_newline.push('\n');
                match highlighter.highlight(&with_newline) {
                    Some(spans) => spans,
                    None => {
                        self.lines = Lines::Plain;
                        vec![Span::raw(body.to_owned())]
                    }
                }
            }
        };
        for _ in 0..std::mem::take(&mut self.held_blank) {
            out.push(code_line(match self.lines {
                Lines::Highlighted(_) | Lines::Shell => Vec::new(),
                Lines::Bare | Lines::Plain => vec![Span::raw("")],
            }));
        }
        out.push(code_line(spans));
    }

    /// What the block shows below its committed lines: the line being typed, after any withheld
    /// blank lines.
    pub(super) fn live(&self, message: &str) -> Vec<Line<'static>> {
        let partial = self.opener.body(&message[self.next_line..]);
        if partial.is_empty() {
            return Vec::new();
        }
        let spans = match &self.lines {
            Lines::Highlighted(highlighter) => highlighter
                .preview(partial)
                .unwrap_or_else(|| vec![Span::raw(partial.to_owned())]),
            Lines::Shell => crate::render::bash_highlight::highlight_bash_spans(partial),
            Lines::Bare | Lines::Plain => vec![Span::raw(partial.to_owned())],
        };
        let mut lines = Vec::with_capacity(self.held_blank + 1);
        for _ in 0..self.held_blank {
            lines.push(code_line(Vec::new()));
        }
        lines.push(code_line(spans));
        lines
    }
}

/// A code line as the renderer draws one: the block's empty indent span, then the line.
fn code_line(spans: Vec<Span<'static>>) -> Line<'static> {
    Line::from_iter(std::iter::once(Span::default()).chain(spans))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn openers_and_closers_follow_commonmark() {
        let opener = FenceOpener::parse("```rust,ignore").expect("backtick opener");
        assert_eq!(opener.lang.as_deref(), Some("rust"));
        assert!(opener.is_closed_by("```"));
        assert!(opener.is_closed_by("   ````  "));
        assert!(!opener.is_closed_by("``"));
        assert!(!opener.is_closed_by("~~~"));
        assert!(!opener.is_closed_by("``` rust"));
        assert!(!opener.is_closed_by("    ```"));

        let tilde = FenceOpener::parse("  ~~~~").expect("tilde opener");
        assert_eq!(tilde.lang, None);
        assert!(!tilde.is_closed_by("~~~"));
        assert!(tilde.is_closed_by("~~~~~"));
        assert_eq!(tilde.body("    x"), "  x");

        assert_eq!(FenceOpener::parse("``` a`b"), None);
        assert_eq!(FenceOpener::parse("    ```"), None);
        assert_eq!(FenceOpener::parse("``"), None);
        assert_eq!(FenceOpener::parse("text"), None);
    }
}
