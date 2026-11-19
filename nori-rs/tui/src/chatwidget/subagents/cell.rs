//! A finished subagent in the transcript: one header, the start of what it concluded, and (when
//! it had helpers of its own) one row counting them. Its whole conversation is in the transcript
//! overlay and the subagent picker, not here.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::SubagentOutcome;
use ratatui::style::Modifier;
use ratatui::text::Line;
use ratatui::text::Span;

use super::FinishedSubagent;
use super::lanes::Counts;
use super::node::NodeId;
use crate::history_cell::HistoryCell;
use crate::history_cell::chrome::Chrome;
use crate::history_cell::chrome::Header;
use crate::history_cell::chrome::Target;
use crate::history_cell::chrome::format_duration;

/// Rows of the child's final message shown in the scrollback summary.
const SUMMARY_MESSAGE_ROWS: usize = 2;

/// One finished subagent: a compact summary for the scrollback, the whole detail for the
/// transcript overlay.
#[derive(Debug)]
pub(crate) struct SubagentSummaryCell {
    finished: FinishedSubagent,
    /// Set by the tree when a child that failed runs again (its fallback route took over): the
    /// card on screen says so instead of reporting a failure that did not end the work.
    revived: Arc<AtomicBool>,
    /// Whether the card has been drawn as revived, so it is redrawn until it has.
    drawn_revived: AtomicBool,
    created: Instant,
}

impl SubagentSummaryCell {
    pub(crate) fn new(finished: FinishedSubagent) -> Self {
        Self {
            finished,
            revived: Arc::default(),
            drawn_revived: AtomicBool::new(false),
            created: Instant::now(),
        }
    }

    /// The card's revival flag, for the tree to set if the child comes back.
    pub(crate) fn revival(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.revived)
    }

    fn is_revived(&self) -> bool {
        let revived = self.revived.load(Ordering::Relaxed);
        if revived {
            self.drawn_revived.store(true, Ordering::Relaxed);
        }
        revived
    }

    #[cfg(test)]
    pub(crate) fn collect_ids(&self, ids: &mut Vec<NodeId>) {
        ids.push(self.finished.node_id);
        for nested in &self.finished.nested {
            nested.collect_ids(ids);
        }
    }

    pub(crate) fn node_id(&self) -> NodeId {
        self.finished.node_id
    }

    /// Count this child and everything that finished under it, by how each ended.
    pub(super) fn add_counts(&self, counts: &mut Counts) {
        match &self.finished.outcome {
            SubagentOutcome::Completed { .. } => counts.done += 1,
            SubagentOutcome::Failed { .. } => counts.failed += 1,
            SubagentOutcome::Cancelled | SubagentOutcome::Closed => counts.cancelled += 1,
        }
        for nested in &self.finished.nested {
            nested.add_counts(counts);
        }
    }

    /// `✓ auth-audit  claude/sonnet-4 · 1 tool · 34s`: the type shown only when it adds to the
    /// task, the duration only when it took a second.
    fn headline(&self, chrome: &Chrome, width: usize) -> Line<'static> {
        let finished = &self.finished;
        let revived = self.is_revived();
        let (glyph, detail) = match &finished.outcome {
            SubagentOutcome::Failed { .. } if revived => (
                Span::styled("↻", chrome.amber()),
                Some("continued on another route".to_owned()),
            ),
            SubagentOutcome::Completed { .. } => (Span::styled("✓", chrome.mint()), None),
            SubagentOutcome::Failed { reason } => (
                Span::styled("✗", chrome.coral().add_modifier(Modifier::BOLD)),
                Some(reason.clone().unwrap_or_else(|| "failed".to_owned())),
            ),
            SubagentOutcome::Cancelled | SubagentOutcome::Closed => (
                Span::styled("⊘", chrome.muted()),
                Some("cancelled".to_owned()),
            ),
        };
        let indent = "  ".repeat(finished.depth);
        let mut meta = Vec::new();
        if finished.tool_count > 0 {
            meta.push(chrome.fact(tool_count_text(finished.tool_count)));
        }
        if let Some(took) = format_duration(finished.elapsed) {
            meta.push(chrome.fact(took));
        }
        if let Some(detail) = detail {
            meta.push(Span::styled(
                detail,
                if revived {
                    chrome.amber()
                } else {
                    chrome.coral()
                },
            ));
        }
        let mut who = chrome.identity(finished.agent.as_deref(), finished.model.as_deref());
        if who.is_empty()
            && let Some(kind) = finished
                .kind
                .as_ref()
                .filter(|kind| **kind != finished.label)
        {
            who.push(Span::styled(kind.clone(), chrome.muted()));
        }
        let mut line = chrome.header(
            Header {
                glyph,
                verb: finished.label.clone(),
                target: if who.is_empty() {
                    Target::None
                } else {
                    Target::Spans(who)
                },
                chip: Vec::new(),
                meta,
            },
            width.saturating_sub(indent.len()),
        );
        line.spans.insert(0, Span::raw(indent));
        line
    }

    /// `⋮ 3 nested · ✓2 ✗1`: what finished under this child, in one row.
    fn nested_summary(&self, chrome: &Chrome) -> Option<Line<'static>> {
        let mut counts = Counts::default();
        for nested in &self.finished.nested {
            nested.add_counts(&mut counts);
        }
        if counts.total() == 0 {
            return None;
        }
        let mut spans = vec![
            Span::raw("  ".repeat(self.finished.depth + 1)),
            Span::styled("⋮ ", chrome.faint()),
            Span::styled(format!("{} nested", counts.total()), chrome.muted()),
            chrome.dot(),
        ];
        spans.extend(counts.spans(chrome));
        Some(Line::from(spans))
    }
}

impl HistoryCell for SubagentSummaryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let mut lines = vec![self.headline(&chrome, usize::from(width))];
        let indent = "  ".repeat(self.finished.depth + 1);
        let message = self.finished.message.trim();
        if !message.is_empty() {
            // The child's answer is markdown; its first rendered rows stand for it, quiet so the
            // root conversation stays the brightest thing on screen.
            let rendered: Vec<Line<'static>> =
                crate::markdown_render::render_markdown_text_with_width(
                    message,
                    Some(usize::from(width).saturating_sub(indent.len()).max(1)),
                )
                .lines
                .into_iter()
                .filter(|line| {
                    line.spans
                        .iter()
                        .any(|span| !span.content.trim().is_empty())
                })
                .collect();
            let total = rendered.len();
            for line in rendered.into_iter().take(SUMMARY_MESSAGE_ROWS) {
                let mut spans = vec![Span::raw(indent.clone())];
                spans.extend(line.spans.into_iter().map(|mut span| {
                    span.style = chrome.muted().patch(span.style);
                    span
                }));
                lines.push(Line::from(spans));
            }
            if total > SUMMARY_MESSAGE_ROWS {
                lines.push(Line::from(vec![
                    Span::raw(indent),
                    Span::styled(
                        format!("⋯ +{} more lines", total - SUMMARY_MESSAGE_ROWS),
                        chrome.faint(),
                    ),
                ]));
            }
        }
        lines.extend(self.nested_summary(&chrome));
        lines
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let mut lines = vec![self.headline(&chrome, usize::from(width))];
        for cell in &self.finished.detail {
            for mut line in cell.transcript_lines(width.saturating_sub(4).max(1)) {
                line.spans.insert(0, "    ".into());
                lines.push(line);
            }
        }
        lines
    }

    fn full_transcript(&self) -> bool {
        true
    }

    /// A failed child's card may flip to "continued" while it is on screen: it is redrawn each
    /// frame until it has, for at most two minutes.
    fn animated(&self) -> bool {
        matches!(self.finished.outcome, SubagentOutcome::Failed { .. })
            && !self.drawn_revived.load(Ordering::Relaxed)
            && self.created.elapsed() < Duration::from_secs(120)
    }
}

pub(crate) fn tool_count_text(count: usize) -> String {
    match count {
        1 => "1 tool".to_owned(),
        count => format!("{count} tools"),
    }
}

/// `7s`, `1m 05s`.
pub(crate) fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}
