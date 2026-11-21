use crate::history_cell::AgentMessageCell;
use crate::history_cell::HistoryCell;
use crate::markdown_stream::MarkdownStreamCollector;
use ratatui::text::Line;

/// Controller for one streaming agent message.
///
/// Deltas are appended as they arrive. Once per frame the owner calls
/// [`Self::commit_for_frame`]: every line that became final since the previous frame is returned
/// in one cell for the scrollback, and [`Self::live_tail_cell`] shows the line still being typed.
/// Nothing is paced by a timer, so a burst of any size is on screen one frame after it arrived.
pub(crate) struct StreamController {
    collector: MarkdownStreamCollector,
    live_width: Option<usize>,
    native_collector: Option<MarkdownStreamCollector>,
    native_complete_line: bool,
    native_pending_work: bool,
    header_emitted: bool,
    boundary: super::boundary::ParagraphBoundary,
}

impl StreamController {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            collector: MarkdownStreamCollector::new(width),
            live_width: width,
            native_collector: width.map(|_| MarkdownStreamCollector::new(None)),
            native_complete_line: false,
            native_pending_work: false,
            header_emitted: false,
            boundary: super::boundary::ParagraphBoundary::default(),
        }
    }

    /// Whether the message so far ends a paragraph, where another cell may go in.
    pub(crate) fn at_paragraph_end(&self) -> bool {
        self.boundary.at_paragraph_end()
    }

    /// Append a delta. Costs the length of the delta.
    pub(crate) fn push(&mut self, delta: &str) {
        self.boundary.push(delta);
        self.collector.push_delta(delta);
        if let Some(native) = &mut self.native_collector {
            native.push_delta(delta);
            self.native_complete_line |= delta.contains('\n');
        }
        // The native collector owns the full source. A long unfinished line
        // with markup or Unicode cannot use the live collector's stable-row
        // shortcut, so periodically retain just a bounded UTF-8-safe suffix
        // for the temporary live view.
        const LIVE_SUFFIX_BYTES: usize = 16 * 1024;
        if let Some(width) = self.live_width
            && self.collector.open_source_len() > LIVE_SUFFIX_BYTES * 2
            && let Some(tail) = self.collector.long_unfinished_tail(LIVE_SUFFIX_BYTES)
        {
            let tail = tail.to_owned();
            self.collector = MarkdownStreamCollector::new(Some(width));
            self.collector.push_delta(&tail);
        }
    }

    /// Whether deltas arrived that no frame has rendered yet.
    pub(crate) fn has_unrendered_input(&self) -> bool {
        self.collector.is_dirty() || self.native_complete_line || self.native_pending_work
    }

    /// Render what arrived since the last frame. Returns the newly final lines as one cell.
    pub(crate) fn commit_for_frame(&mut self) -> Option<Box<dyn HistoryCell>> {
        let lines = self.collector.advance();
        let lines = if let Some(native) = &mut self.native_collector {
            if std::mem::take(&mut self.native_complete_line) || self.native_pending_work {
                let lines = native.advance();
                self.native_pending_work = native.is_dirty();
                lines
            } else {
                Vec::new()
            }
        } else {
            lines
        };
        self.emit(lines)
    }

    /// The rendered but not yet final lines, with the prefix their committed form will have.
    pub(crate) fn live_tail_cell(&self) -> Option<AgentMessageCell> {
        let lines = self.collector.live_tail();
        let start = if self.live_width.is_some() {
            lines.len().saturating_sub(8)
        } else {
            0
        };
        (!lines.is_empty())
            .then(|| AgentMessageCell::new(lines[start..].to_vec(), !self.header_emitted))
    }

    /// Finalize the active stream. Drain and emit now.
    pub(crate) fn finalize(&mut self) -> (Option<Box<dyn HistoryCell>>, Option<String>) {
        let (lines, source) = if let Some(native) = &mut self.native_collector {
            native.finalize_and_drain()
        } else {
            self.collector.finalize_and_drain()
        };
        let cell = self.emit(lines);
        let source = (!source.is_empty()).then_some(source);
        (cell, source)
    }

    /// Keep shutdown responsive for a huge unfinished line. The ACP journal
    /// retains the complete source; terminal history receives its recent tail.
    pub(crate) fn finish_for_exit(&mut self) -> Option<Box<dyn HistoryCell>> {
        const MAX_EXIT_TAIL_BYTES: usize = 16 * 1024;
        let collector = self.native_collector.as_ref().unwrap_or(&self.collector);
        if let Some(tail) = collector.long_unfinished_tail(MAX_EXIT_TAIL_BYTES) {
            let tail: String = tail
                .chars()
                .filter(|ch| !ch.is_control() || *ch == '\t')
                .collect();
            let cell =
                AgentMessageCell::new(vec![Line::from(format!("…{tail}"))], !self.header_emitted);
            self.header_emitted = true;
            return Some(Box::new(cell));
        }
        if collector.open_source_len() > MAX_EXIT_TAIL_BYTES
            || collector.open_source_has_many_lines(128)
        {
            let cell = AgentMessageCell::new(
                vec![Line::from(
                    "[Remaining streamed output is in the conversation journal]",
                )],
                !self.header_emitted,
            );
            self.header_emitted = true;
            return Some(Box::new(cell));
        }
        self.finalize().0
    }

    fn emit(&mut self, lines: Vec<Line<'static>>) -> Option<Box<dyn HistoryCell>> {
        if lines.is_empty() {
            return None;
        }
        let is_first_line = !self.header_emitted;
        self.header_emitted = true;
        Some(Box::new(AgentMessageCell::new(lines, is_first_line)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_stream_keeps_live_tail_bounded_and_commits_one_logical_line() {
        let mut controller = StreamController::new(Some(32));
        let part = "wide café 界 text ";
        for _ in 0..100 {
            controller.push(part);
            assert!(controller.commit_for_frame().is_none());
            if let Some(live) = controller.live_tail_cell() {
                assert!(live.display_lines(32).len() < 20);
            }
        }
        controller.push("end\n");
        let cell = controller.commit_for_frame().expect("completed paragraph");
        let lines = cell.native_lines(32);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), format!("• {}end", part.repeat(100)));
    }

    #[test]
    fn finalizing_a_partial_stream_emits_only_its_uncommitted_tail() {
        let mut controller = StreamController::new(Some(20));
        controller.push("first completed paragraph\n\n");
        let first = controller.commit_for_frame().expect("first paragraph");
        assert!(
            first
                .native_lines(20)
                .iter()
                .any(|line| line.to_string().contains("first completed"))
        );
        controller.push("second unfinished paragraph");
        assert!(controller.commit_for_frame().is_none());
        let tail = controller.finalize().0.expect("unfinished tail");
        let text = tail
            .native_lines(20)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("second unfinished paragraph"));
        assert!(!text.contains("first completed paragraph"));
    }

    #[test]
    fn huge_unicode_live_source_is_bounded_without_truncating_native_output() {
        let mut controller = StreamController::new(Some(32));
        let source = "Unicode café 界 ".repeat(3_000);
        controller.push(&source);
        assert!(controller.commit_for_frame().is_none());
        assert!(controller.collector.open_source_len() <= 16 * 1024);
        controller.push("end\n");
        let cell = controller.commit_for_frame().expect("completed paragraph");
        let lines = cell.native_lines(32);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].to_string(), format!("• {source}end"));
    }

    #[test]
    fn exit_limits_the_terminal_tail_of_a_huge_unfinished_line() {
        let mut controller = StreamController::new(Some(40));
        controller.push(&format!("{}終", "café ".repeat(100_000)));
        let cell = controller.finish_for_exit().expect("bounded tail");
        let lines = cell.native_lines(40);
        let text = lines[0].to_string();
        assert!(text.ends_with('終'));
        assert!(text.len() <= 16 * 1024 + 16);
        assert!(text.contains('…'));
    }

    #[test]
    fn exit_does_not_parse_a_huge_pending_multiline_block() {
        let mut controller = StreamController::new(Some(40));
        controller.push(&"x\n".repeat(2_000));
        let cell = controller.finish_for_exit().expect("journal notice");
        assert!(
            cell.native_lines(40)[0]
                .to_string()
                .contains("conversation journal")
        );
    }

    fn lines_to_plain_strings(lines: &[ratatui::text::Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.clone())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect()
    }

    #[tokio::test]
    async fn controller_loose_vs_tight_with_commit_ticks_matches_full() {
        let mut ctrl = StreamController::new(None);
        let mut lines = Vec::new();

        // Exact deltas from the session log (section: Loose vs. tight list items)
        let deltas = vec![
            "\n\n",
            "Loose",
            " vs",
            ".",
            " tight",
            " list",
            " items",
            ":\n",
            "1",
            ".",
            " Tight",
            " item",
            "\n",
            "2",
            ".",
            " Another",
            " tight",
            " item",
            "\n\n",
            "1",
            ".",
            " Loose",
            " item",
            " with",
            " its",
            " own",
            " paragraph",
            ".\n\n",
            "  ",
            " This",
            " paragraph",
            " belongs",
            " to",
            " the",
            " same",
            " list",
            " item",
            ".\n\n",
            "2",
            ".",
            " Second",
            " loose",
            " item",
            " with",
            " a",
            " nested",
            " list",
            " after",
            " a",
            " blank",
            " line",
            ".\n\n",
            "  ",
            " -",
            " Nested",
            " bullet",
            " under",
            " a",
            " loose",
            " item",
            "\n",
            "  ",
            " -",
            " Another",
            " nested",
            " bullet",
            "\n\n",
        ];

        // Simulate streaming with a frame after each delta.
        for d in deltas.iter() {
            ctrl.push(d);
            if let Some(cell) = ctrl.commit_for_frame() {
                lines.extend(cell.transcript_lines(u16::MAX));
            }
        }
        // Finalize and flush remaining lines now.
        if let Some(cell) = ctrl.finalize().0 {
            lines.extend(cell.transcript_lines(u16::MAX));
        }

        let streamed: Vec<_> = lines_to_plain_strings(&lines)
            .into_iter()
            // skip • and 2-space indentation
            .map(|s| s.chars().skip(2).collect::<String>())
            .collect();

        // Full render of the same source
        let source: String = deltas.iter().copied().collect();
        let mut rendered: Vec<ratatui::text::Line<'static>> = Vec::new();
        crate::markdown::append_markdown(&source, None, &mut rendered);
        let rendered_strs = lines_to_plain_strings(&rendered);

        assert_eq!(streamed, rendered_strs);

        // Also assert exact expected plain strings for clarity.
        let expected = vec![
            "Loose vs. tight list items:".to_string(),
            "".to_string(),
            "1. Tight item".to_string(),
            "2. Another tight item".to_string(),
            "3. Loose item with its own paragraph.".to_string(),
            "".to_string(),
            "   This paragraph belongs to the same list item.".to_string(),
            "4. Second loose item with a nested list after a blank line.".to_string(),
            "    - Nested bullet under a loose item".to_string(),
            "    - Another nested bullet".to_string(),
        ];
        assert_eq!(
            streamed, expected,
            "expected exact rendered lines for loose/tight section"
        );
    }

    #[test]
    fn finalize_returns_the_complete_raw_markdown_source() {
        let mut controller = StreamController::new(Some(20));
        controller.push("A **formatted** line\n");
        let _ = controller.commit_for_frame();
        controller.push("and a final fragment");

        let (_cell, source) = controller.finalize();

        assert_eq!(
            source.as_deref(),
            Some("A **formatted** line\nand a final fragment")
        );
    }
}
