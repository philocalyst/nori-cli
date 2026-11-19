//! What the monitors an agent armed reported to it, one row per monitor:
//! `◉ build · 3 events · FAILED: error[E0308] mismatched types`.

use nori_protocol::MonitorDigest;
use ratatui::text::Line;
use ratatui::text::Span;

use super::HistoryCell;
use super::chrome::Chrome;
use super::chrome::GUTTER;
use super::chrome::spans_width;
use super::chrome::truncate_tail;

/// A batch of monitor events handed to the agent.
#[derive(Debug)]
pub(crate) struct MonitorEventsCell {
    digests: Vec<MonitorDigest>,
}

impl MonitorEventsCell {
    pub(crate) fn new(digests: Vec<MonitorDigest>) -> Self {
        Self { digests }
    }
}

impl HistoryCell for MonitorEventsCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let chrome = Chrome::live();
        let width = usize::from(width);
        self.digests
            .iter()
            .map(|digest| {
                let tone = if digest.failed {
                    chrome.coral()
                } else if digest.ended.is_some() {
                    chrome.mint()
                } else {
                    chrome.cyan()
                };
                let noun = if digest.events == 1 {
                    "event"
                } else {
                    "events"
                };
                let mut spans = vec![
                    Span::styled("◉", tone),
                    Span::raw(" "),
                    Span::styled(digest.label.clone(), chrome.strong()),
                    chrome.dot(),
                    chrome.fact(format!("{} {noun}", digest.events)),
                ];
                if let Some(ended) = &digest.ended {
                    spans.push(chrome.dot());
                    spans.push(chrome.fact(ended.clone()));
                }
                if !digest.headline.is_empty() {
                    spans.push(chrome.dot());
                    let room = width.saturating_sub(spans_width(&spans) + GUTTER).max(8);
                    let headline = digest.headline.replace('\n', " ");
                    if digest.failed {
                        spans.push(Span::styled("FAILED: ", chrome.coral()));
                        spans.push(Span::styled(
                            truncate_tail(&headline, room.saturating_sub(8)),
                            chrome.secondary(),
                        ));
                    } else {
                        spans.push(Span::styled(
                            truncate_tail(&headline, room),
                            chrome.secondary(),
                        ));
                    }
                }
                Line::from(spans)
            })
            .collect()
    }
}
