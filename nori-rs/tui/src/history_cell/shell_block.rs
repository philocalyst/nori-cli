//! A shell command and what it printed: the shape shared by the agent's exec calls and the
//! user's own `!` commands.
//!
//! ```text
//! $ cargo test -p nori-harness -- wake_fence                         1.4s
//!   test cancel_during_wake_retains_child_verdict … ok
//!   18 passed
//! ```
//!
//! The `$` is the status glyph (a spinner while it runs, coral when it failed). The command is
//! highlighted by the one shell highlighter and wraps under itself; its duration (when it took a
//! second) and working directory sit at the right if there is room; a non-zero exit is a chip
//! that is never dropped. Output is its tail: collapsed, the last row or two, or the last few
//! when the command failed; expanded, more, with the count of what was left out.

use std::time::Duration;
use std::time::Instant;

use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

use super::chrome::Chrome;
use super::chrome::Disclosure;
use super::chrome::GUTTER;
use super::chrome::Progress;
use super::chrome::format_duration;
use super::chrome::spans_width;
use super::chrome::truncate_tail;
use crate::render::bash_highlight::ShellStyles;
use crate::render::bash_highlight::highlight_shell_lines;
use crate::wrapping::RtOptions;
use crate::wrapping::word_wrap_lines;

/// Rows of output a collapsed block shows when the command succeeded.
const TAIL_OK: usize = 2;
/// Rows of output a collapsed block shows when the command failed: the reason is in the tail.
const TAIL_FAILED: usize = 6;
/// Rows of output an expanded block shows at most.
const TAIL_EXPANDED: usize = 40;
/// Rows of a long command a collapsed block shows before cutting it.
const COMMAND_ROWS: usize = 3;

/// Everything the block draws.
pub(crate) struct ShellBlock<'a> {
    pub(crate) command: &'a str,
    pub(crate) progress: Progress,
    pub(crate) since: Option<Instant>,
    pub(crate) animate: bool,
    /// How long it took, once it has finished and the time is known.
    pub(crate) elapsed: Option<Duration>,
    pub(crate) exit_code: Option<i32>,
    /// Where it ran, when that is worth saying (the user's shell moves; an agent's does not).
    pub(crate) cwd: Option<&'a str>,
    pub(crate) stdout: &'a str,
    pub(crate) stderr: &'a str,
}

impl ShellBlock<'_> {
    pub(crate) fn render(
        &self,
        chrome: &Chrome,
        width: usize,
        disclosure: Disclosure,
    ) -> Vec<Line<'static>> {
        let mut lines = self.command_rows(chrome, width, disclosure);
        lines.extend(self.output_rows(chrome, width, disclosure));
        lines
    }

    fn failed(&self) -> bool {
        self.progress == Progress::Failed || self.exit_code.is_some_and(|code| code != 0)
    }

    fn command_rows(
        &self,
        chrome: &Chrome,
        width: usize,
        disclosure: Disclosure,
    ) -> Vec<Line<'static>> {
        let styles = ShellStyles::from_theme(&chrome.theme);
        let progress = if self.failed() {
            Progress::Failed
        } else {
            self.progress
        };
        let glyph = match progress {
            Progress::Running => chrome.glyph(Progress::Running, self.since, self.animate),
            _ => Span::styled(
                "$",
                chrome
                    .tone(progress)
                    .add_modifier(ratatui::style::Modifier::BOLD),
            ),
        };
        let text_width = width.saturating_sub(GUTTER).max(1);
        let command = self.command.trim_end();
        let mut logical: Vec<Line<'static>> = highlight_shell_lines(command, &styles)
            .into_iter()
            .map(Line::from)
            .collect();
        if logical.is_empty() {
            logical.push(Line::default());
        }
        let mut rows = word_wrap_lines(
            logical,
            RtOptions::new(text_width + GUTTER)
                .initial_indent(Line::from(vec![glyph, Span::raw(" ")]))
                .subsequent_indent("  ".into()),
        );
        if disclosure == Disclosure::Collapsed && rows.len() > COMMAND_ROWS {
            let cut = rows.len() - COMMAND_ROWS;
            rows.truncate(COMMAND_ROWS);
            if let Some(last) = rows.last_mut() {
                last.spans
                    .push(Span::styled(format!(" … +{cut} rows"), chrome.faint()));
            }
        }

        // Facts that go at the right of the last row.
        let chip = if self.failed() {
            Some(Span::styled(
                match self.exit_code {
                    Some(code) => format!("exit {code}"),
                    None => "failed".to_owned(),
                },
                chrome.coral().add_modifier(ratatui::style::Modifier::BOLD),
            ))
        } else if self.progress == Progress::Cancelled {
            Some(Span::styled("cancelled", chrome.muted()))
        } else {
            None
        };
        let took = match self.progress {
            Progress::Running => self.since.map(|since| since.elapsed()),
            Progress::Waiting
            | Progress::Done
            | Progress::Failed
            | Progress::Cancelled
            | Progress::Idle => self.elapsed,
        }
        .and_then(format_duration);
        let mut facts: Vec<Span<'static>> = Vec::new();
        if let Some(cwd) = self.cwd {
            facts.push(Span::styled(cwd.to_owned(), chrome.faint()));
        }
        if let Some(took) = took {
            facts.push(Span::styled(took, chrome.muted()));
        }
        if let Some(chip) = chip.clone() {
            facts.push(chip);
        }
        // Everything if it fits, else the facts that matter most, else the chip on its own row.
        let fits = |facts: &[Span<'static>], used: usize| {
            let needed = facts
                .iter()
                .map(|fact| fact.content.chars().count() + 2)
                .sum::<usize>();
            used + needed <= width
        };
        let used = rows.last().map_or(0, |row| spans_width(&row.spans));
        let mut tail: Vec<Span<'static>> = Vec::new();
        for take in [
            facts.len(),
            facts.len().saturating_sub(1),
            usize::from(chip.is_some()),
        ] {
            let candidate: Vec<Span<'static>> =
                facts[facts.len() - take.min(facts.len())..].to_vec();
            if fits(&candidate, used) {
                tail = candidate;
                break;
            }
        }
        if tail.is_empty()
            && let Some(chip) = chip
        {
            rows.push(Line::from(vec![Span::raw("  "), chip]));
        }
        if let Some(last) = rows.last_mut().filter(|_| !tail.is_empty()) {
            let needed: usize = tail
                .iter()
                .map(|fact| fact.content.chars().count() + 2)
                .sum();
            let gap = width.saturating_sub(used + needed).max(2);
            last.spans.push(Span::raw(" ".repeat(gap)));
            for (index, fact) in tail.into_iter().enumerate() {
                if index > 0 {
                    last.spans.push(Span::raw("  "));
                }
                last.spans.push(fact);
            }
        }

        // The command and its output are one block: on the well, where there is one.
        if chrome.has_surfaces() {
            rows.into_iter()
                .map(|mut row| {
                    let used = spans_width(&row.spans);
                    if used < width {
                        row.spans.push(Span::raw(" ".repeat(width - used)));
                    }
                    for span in &mut row.spans {
                        span.style = chrome.theme.code_well.patch(span.style);
                    }
                    row.style(chrome.theme.code_well)
                })
                .collect()
        } else {
            rows
        }
    }

    fn output_rows(
        &self,
        chrome: &Chrome,
        width: usize,
        disclosure: Disclosure,
    ) -> Vec<Line<'static>> {
        let stdout = plain_lines(self.stdout);
        let stderr = plain_lines(self.stderr);
        let failed = self.failed();
        let mut rows: Vec<(String, bool)> = stdout.into_iter().map(|line| (line, false)).collect();
        rows.extend(stderr.into_iter().map(|line| (line, failed)));
        if rows.is_empty() {
            return Vec::new();
        }
        let keep = match (disclosure, failed) {
            (Disclosure::Expanded, _) => TAIL_EXPANDED,
            (Disclosure::Collapsed, true) => TAIL_FAILED,
            (Disclosure::Collapsed, false) => TAIL_OK,
        };
        // One hidden row costs a row to say so: show it instead.
        let earlier = match rows.len().saturating_sub(keep) {
            1 => 0,
            hidden => hidden,
        };
        let tail = rows.split_off(earlier);
        let room = width.saturating_sub(chrome.body_indent()).max(1);
        let text: Style = chrome.secondary();
        let mut spans_rows: Vec<Vec<Span<'static>>> = Vec::new();
        if earlier > 0 {
            spans_rows.push(vec![Span::styled(
                format!("⋮ {earlier} earlier lines"),
                chrome.faint(),
            )]);
        }
        for (line, is_error) in tail {
            let style = if is_error { chrome.coral() } else { text };
            match disclosure {
                Disclosure::Collapsed => {
                    spans_rows.push(vec![Span::styled(truncate_tail(&line, room), style)]);
                }
                Disclosure::Expanded => {
                    let wrapped = word_wrap_lines(
                        [Line::from(Span::styled(line, style))],
                        RtOptions::new(room),
                    );
                    spans_rows.extend(wrapped.into_iter().map(|row| row.spans));
                }
            }
        }
        if chrome.has_surfaces() {
            chrome.well(spans_rows, width)
        } else {
            spans_rows
                .into_iter()
                .map(|mut spans| {
                    spans.insert(0, chrome.body_prefix());
                    Line::from(spans)
                })
                .collect()
        }
    }
}

/// Output as plain rows: escape sequences and control characters removed, tabs expanded, blank
/// rows at the end dropped.
fn plain_lines(text: &str) -> Vec<String> {
    let mut rows: Vec<String> = text.lines().map(strip_controls).collect();
    while rows.last().is_some_and(|row| row.trim().is_empty()) {
        rows.pop();
    }
    rows
}

fn strip_controls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\u{1b}' => match chars.peek() {
                // CSI: parameters, then a final byte in `@`..=`~`.
                Some('[') => {
                    chars.next();
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                // OSC: up to BEL or ST.
                Some(']') => {
                    chars.next();
                    while let Some(next) = chars.next() {
                        if next == '\u{7}'
                            || (next == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                        {
                            break;
                        }
                    }
                }
                _ => {
                    chars.next();
                }
            },
            '\t' => out.push_str("    "),
            '\r' => {}
            other if other.is_control() => {}
            other => out.push(other),
        }
    }
    out
}
