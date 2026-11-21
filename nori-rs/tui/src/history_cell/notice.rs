//! What Nori itself says, as opposed to what the agent says.
//!
//! A notice has its own glyph in the gutter (`◇` information, `▲` warning, `✗` error) and quiet
//! text, so it can never be mistaken for the model's prose, and notices that arrive together are
//! one cell with one blank row above them instead of a blank row each.

use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;
use unicode_width::UnicodeWidthStr;

use super::HistoryCell;
use super::chrome::Chrome;
use super::chrome::GUTTER;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoticeKind {
    Info,
    Warning,
    Error,
}

/// One thing Nori says. `hint` is the quieter second half: the command that undoes it, the
/// reason behind it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) kind: NoticeKind,
    pub(crate) text: String,
    pub(crate) hint: Option<String>,
}

/// One or more notices, stacked.
#[derive(Debug)]
pub(crate) struct NoticeCell {
    notices: Vec<Notice>,
}

impl NoticeCell {
    pub(crate) fn new(notice: Notice) -> Self {
        Self {
            notices: vec![notice],
        }
    }

    /// Add a notice that arrived right after the last. Saying the same thing twice in a row
    /// says it once.
    pub(crate) fn push(&mut self, notice: Notice) {
        if self.notices.last() != Some(&notice) {
            self.notices.push(notice);
        }
    }
}

impl HistoryCell for NoticeCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = usize::from(width);
        let chrome = Chrome::live();
        let mut out = Vec::new();
        for notice in &self.notices {
            let (glyph, text_style) = match notice.kind {
                NoticeKind::Info => ("◇", chrome.muted()),
                NoticeKind::Warning => ("▲", chrome.amber()),
                NoticeKind::Error => ("✗", chrome.coral().add_modifier(Modifier::BOLD)),
            };
            let glyph_style = match notice.kind {
                NoticeKind::Info => chrome.violet(),
                NoticeKind::Warning => chrome.amber(),
                NoticeKind::Error => chrome.coral(),
            };
            let mut spans = vec![Span::styled(notice.text.clone(), text_style)];
            if let Some(hint) = &notice.hint {
                spans.push(chrome.dot());
                spans.push(Span::styled(hint.clone(), chrome.faint()));
            }
            let line = Line::from(spans);
            let indent = Line::from(vec![Span::styled(glyph, glyph_style), Span::raw(" ")]);
            out.extend(word_wrap_lines(
                [line],
                RtOptions::new(width.max(GUTTER + 1))
                    .initial_indent(indent)
                    .subsequent_indent("  ".into()),
            ));
        }
        out
    }
}

pub(crate) fn new_info_event(message: String, hint: Option<String>) -> NoticeCell {
    NoticeCell::new(Notice {
        kind: NoticeKind::Info,
        text: message,
        hint,
    })
}

pub(crate) fn new_error_event(message: String) -> NoticeCell {
    NoticeCell::new(Notice {
        kind: NoticeKind::Error,
        text: message,
        hint: None,
    })
}

pub(crate) fn new_skillset_switched_event(name: &str) -> NoticeCell {
    new_info_event(format!("Switched to skillset \"{name}\""), None)
}

/// Rows of remote-control status: the first states it, the rest are its details.
pub(crate) fn new_remote_control_event(lines: &[String]) -> NoticeCell {
    let mut lines = lines.iter();
    let head = lines.next().cloned().unwrap_or_default();
    let detail = lines.cloned().collect::<Vec<_>>().join(" · ");
    new_info_event(head, (!detail.is_empty()).then_some(detail))
}

/// Lines drawn exactly as given.
#[derive(Debug)]
pub(crate) struct PlainHistoryCell {
    lines: Vec<Line<'static>>,
}

impl PlainHistoryCell {
    pub(crate) fn new(lines: Vec<Line<'static>>) -> Self {
        Self { lines }
    }
}

impl HistoryCell for PlainHistoryCell {
    fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
        self.lines.clone()
    }
}

/// The rule between a turn's work and its answer. A turn that took a few seconds needs no
/// marker (the status line says how long it took), so this draws only when the work was long.
#[derive(Debug)]
pub struct FinalMessageSeparator {
    elapsed_seconds: Option<u64>,
}

/// Turns shorter than this get no separator.
const SEPARATOR_MIN_SECONDS: u64 = 10;

impl FinalMessageSeparator {
    pub(crate) fn new(elapsed_seconds: Option<u64>) -> Self {
        Self { elapsed_seconds }
    }

    /// Whether a turn of this length is worth marking.
    pub(crate) fn marks(elapsed_seconds: Option<u64>) -> bool {
        elapsed_seconds.is_some_and(|seconds| seconds >= SEPARATOR_MIN_SECONDS)
    }
}

impl HistoryCell for FinalMessageSeparator {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let Some(seconds) = self
            .elapsed_seconds
            .filter(|_| Self::marks(self.elapsed_seconds))
        else {
            return Vec::new();
        };
        let chrome = Chrome::live();
        let label = format!(
            " {} ",
            crate::status_indicator_widget::fmt_elapsed_compact(seconds)
        );
        let rule = usize::from(width).saturating_sub(label.width() + GUTTER);
        vec![Line::from(vec![
            Span::styled("──", chrome.faint()),
            Span::styled(label, chrome.muted()),
            Span::styled("─".repeat(rule), chrome.faint()),
        ])]
    }
}
