//! The words in a transcript: the user's prompt, the model's reasoning and the model's answer.
//!
//! The prompt sits on the prompt surface behind a violet rail; reasoning is one quiet violet-led
//! row; an answer is plain text with two columns of air on its left, the same two columns a tool
//! header's glyph takes, so everything hangs from one edge.

use std::path::Path;
use std::path::PathBuf;

use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::HistoryCell;
use super::chrome::Chrome;
use super::chrome::GUTTER;
use super::chrome::spans_width;
use crate::markdown::append_markdown;
use crate::render::line_utils::prefix_lines;
use crate::ui_consts::LIVE_PREFIX_COLS;
use crate::wrapping::RtOptions;
use crate::wrapping::adaptive_wrap_lines;
use crate::wrapping::word_wrap_lines;

/// What the user said.
#[derive(Debug)]
pub(crate) struct UserHistoryCell {
    pub message: String,
}

impl UserHistoryCell {
    /// Rows for `lines` of the prompt: on the prompt surface behind a rail where the terminal
    /// can paint one, behind a plain `›` where it cannot. `width` pads surface rows to a clean
    /// block; scrollback, which may reflow, passes `None`.
    fn framed(&self, lines: Vec<Line<'static>>, width: Option<usize>) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let surface = chrome.theme.prompt_surface;
        let strong = chrome.strong();
        let body: Vec<Line<'static>> = lines
            .into_iter()
            .map(|mut line| {
                for span in &mut line.spans {
                    span.style = strong.patch(span.style);
                }
                line
            })
            .collect();
        if surface.bg.is_none() {
            // No surface to pad: the blank row between cells is the only space around it.
            return prefix_lines(
                body,
                Span::styled("› ", chrome.muted().add_modifier(Modifier::BOLD)),
                "  ".into(),
            );
        }
        let rail = Span::styled("▌ ", chrome.violet());
        let framed = |mut spans: Vec<Span<'static>>| {
            if let Some(width) = width {
                let used = spans_width(&spans);
                if used < width {
                    spans.push(Span::raw(" ".repeat(width - used)));
                }
            }
            Line::from(spans).style(surface)
        };
        let mut out = vec![framed(vec![rail.clone()])];
        out.extend(body.into_iter().map(|line| {
            let mut spans = vec![rail.clone()];
            spans.extend(line.spans);
            framed(spans)
        }));
        out.push(framed(vec![rail]));
        out
    }
}

impl HistoryCell for UserHistoryCell {
    fn native_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let lines = self.message.lines().map(|line| Line::from(line.to_owned()));
        self.framed(lines.collect(), None)
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        // One column stays free on the right so a wrapped word never touches the edge.
        let wrap_width = width.saturating_sub(LIVE_PREFIX_COLS + 1).max(1);
        let wrapped = word_wrap_lines(
            self.message.lines().map(Line::from),
            // Wrap algorithm matches textarea.rs.
            RtOptions::new(usize::from(wrap_width))
                .wrap_algorithm(textwrap::WrapAlgorithm::FirstFit),
        );
        self.framed(wrapped, Some(usize::from(width)))
    }
}

/// The model's reasoning, summarised to one quiet row.
#[derive(Debug)]
pub(crate) struct ReasoningSummaryCell {
    content: String,
    transcript_only: bool,
}

impl ReasoningSummaryCell {
    pub(crate) fn new(content: String, transcript_only: bool) -> Self {
        Self {
            content,
            transcript_only,
        }
    }

    fn quiet(&self, wrap: Option<usize>) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let mut lines: Vec<Line<'static>> = Vec::new();
        append_markdown(&self.content, wrap, &mut lines);
        let quiet: Style = chrome.muted().add_modifier(Modifier::ITALIC);
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = span.style.patch(quiet);
            }
        }
        lines
    }

    fn lines(&self, width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let quiet = self.quiet(Some(usize::from(width).saturating_sub(GUTTER)));
        word_wrap_lines(
            &quiet,
            RtOptions::new(usize::from(width))
                .initial_indent(Span::styled("✦ ", chrome.violet()).into())
                .subsequent_indent("  ".into()),
        )
    }
}

impl HistoryCell for ReasoningSummaryCell {
    fn native_lines(&self, _width: u16) -> Vec<Line<'static>> {
        if self.transcript_only {
            return Vec::new();
        }
        let chrome = Chrome::live();
        prefix_lines(
            self.quiet(None),
            Span::styled("✦ ", chrome.violet()),
            "  ".into(),
        )
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        if self.transcript_only {
            Vec::new()
        } else {
            self.lines(width)
        }
    }

    fn desired_height(&self, width: u16) -> u16 {
        u16::try_from(self.display_lines(width).len()).unwrap_or(u16::MAX)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.lines(width)
    }
}

/// A streamed piece of the model's answer.
#[derive(Debug)]
pub(crate) struct AgentMessageCell {
    lines: Vec<Line<'static>>,
    is_first_line: bool,
}

impl AgentMessageCell {
    pub(crate) fn new(lines: Vec<Line<'static>>, is_first_line: bool) -> Self {
        Self {
            lines,
            is_first_line,
        }
    }
}

impl HistoryCell for AgentMessageCell {
    fn native_lines(&self, _width: u16) -> Vec<Line<'static>> {
        prefix_lines(self.lines.clone(), "  ".into(), "  ".into())
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        adaptive_wrap_lines(
            &self.lines,
            RtOptions::new(usize::from(width))
                .initial_indent("  ".into())
                .subsequent_indent("  ".into()),
        )
    }

    fn is_stream_continuation(&self) -> bool {
        !self.is_first_line
    }
}

/// A completed answer that keeps its Markdown so it can be laid out again at any width.
#[derive(Debug)]
pub(crate) struct AgentMarkdownCell {
    markdown_source: String,
    cwd: PathBuf,
    /// The layout at the width it was last asked for. A cell is laid out again only when the
    /// width changes, so replaying a transcript at an unchanged width costs a copy of the rows.
    layout: std::sync::Mutex<Option<(u16, Vec<Line<'static>>)>>,
}

impl AgentMarkdownCell {
    pub(crate) fn new(markdown_source: String, cwd: &Path) -> Self {
        Self {
            markdown_source,
            cwd: cwd.to_path_buf(),
            layout: std::sync::Mutex::new(None),
        }
    }
}

impl HistoryCell for AgentMarkdownCell {
    fn native_lines(&self, _width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        crate::markdown::append_markdown_with_cwd(
            &self.markdown_source,
            None,
            Some(&self.cwd),
            &mut lines,
        );
        prefix_lines(lines, "  ".into(), "  ".into())
    }

    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut layout = self
            .layout
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((laid_out_width, lines)) = layout.as_ref()
            && *laid_out_width == width
        {
            return lines.clone();
        }
        let mut lines = Vec::new();
        crate::markdown::append_markdown_with_cwd(
            &self.markdown_source,
            Some(usize::from(width.saturating_sub(2).max(1))),
            Some(&self.cwd),
            &mut lines,
        );
        let lines = prefix_lines(lines, "  ".into(), "  ".into());
        *layout = Some((width, lines.clone()));
        lines
    }
}
