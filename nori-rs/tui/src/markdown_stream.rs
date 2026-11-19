use std::path::PathBuf;

use ratatui::text::Line;

use crate::markdown_render::committable_prefix_len;
use crate::markdown_render::markdown_segments;
use crate::markdown_render::render_markdown_continuation;
use crate::markdown_render::top_level_block_starts;
use crate::render::line_utils::is_blank_line_spaces_only;

mod fence;
mod long_line;

pub(crate) use fence::FenceOpener;

/// Incremental markdown renderer for a message that is still streaming.
///
/// Completed top-level blocks are frozen: rendered once, their lines handed out, and their source
/// never parsed again. Only the open block (the last one, which more text can still change) is
/// re-rendered, so handling a delta costs the delta plus the open block, not the message.
///
/// Within the open block, lines that end in a newline are committed (handed out once, for the
/// scrollback), and the rest are the live tail: what the line being typed currently looks like.
///
/// A fenced code block is streamed by [`fence::OpenFence`] instead: each line is highlighted and
/// committed once, so a long code block costs its length in total rather than on every frame.
pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    width: Option<usize>,
    cwd: Option<PathBuf>,
    /// Bytes of `buffer` that are frozen. The open block starts here.
    frozen_len: usize,
    /// The frozen text rendered something, so the open block continues that output and opens
    /// with the separator a block after another gets.
    prose_continues: bool,
    /// Lines of the open block already handed out.
    committed_in_open: usize,
    /// Length of the open block's complete prefix that was last rendered.
    complete_cache: Option<usize>,
    /// Where the line being typed starts, and where the first of its rows not committed yet
    /// starts, when some of them are (see `long_line`).
    line_committed: Option<(usize, usize)>,
    live: Vec<Line<'static>>,
    dirty: bool,
    /// The open block is a fenced code block streamed line by line.
    fence: Option<fence::OpenFence>,
    /// Bytes handed to the markdown parser so far, for work-counting tests.
    parsed_bytes: usize,
    /// Bytes highlighted by fences that have closed, for work-counting tests.
    highlighted_bytes: usize,
}

impl MarkdownStreamCollector {
    pub fn new(width: Option<usize>) -> Self {
        Self::with_cwd(width, None)
    }

    #[allow(dead_code)]
    pub fn new_with_cwd(width: Option<usize>, cwd: PathBuf) -> Self {
        Self::with_cwd(width, Some(cwd))
    }

    fn with_cwd(width: Option<usize>, cwd: Option<PathBuf>) -> Self {
        Self {
            buffer: String::new(),
            width,
            cwd,
            frozen_len: 0,
            prose_continues: false,
            committed_in_open: 0,
            complete_cache: None,
            line_committed: None,
            live: Vec::new(),
            dirty: false,
            fence: None,
            parsed_bytes: 0,
            highlighted_bytes: 0,
        }
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.frozen_len = 0;
        self.prose_continues = false;
        self.committed_in_open = 0;
        self.complete_cache = None;
        self.line_committed = None;
        self.live.clear();
        self.dirty = false;
        self.fence = None;
    }

    /// Append a delta. Costs the length of the delta; rendering waits for [`Self::advance`].
    pub fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
        self.dirty = true;
    }

    /// The line being typed and anything else rendered but not yet committed, as of the last
    /// [`Self::advance`].
    pub fn live_tail(&self) -> &[Line<'static>] {
        &self.live
    }

    /// Whether a delta arrived that [`Self::advance`] has not rendered yet.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// A bounded suffix of an exceptionally long unfinished source line.
    pub(crate) fn long_unfinished_tail(&self, max_bytes: usize) -> Option<&str> {
        let open = &self.buffer[self.frozen_len..];
        let line = memchr::memrchr(b'\n', open.as_bytes()).map_or(open, |index| &open[index + 1..]);
        if line.len() <= max_bytes {
            return None;
        }
        let mut start = line.len() - max_bytes;
        while !line.is_char_boundary(start) {
            start += 1;
        }
        Some(&line[start..])
    }

    pub(crate) fn open_source_len(&self) -> usize {
        self.buffer.len().saturating_sub(self.frozen_len)
    }

    pub(crate) fn open_source_has_many_lines(&self, limit: usize) -> bool {
        memchr::memchr_iter(b'\n', &self.buffer.as_bytes()[self.frozen_len..])
            .take(limit.saturating_add(1))
            .count()
            > limit
    }

    /// Bytes handed to the markdown parser so far.
    #[cfg(test)]
    pub fn parsed_bytes(&self) -> usize {
        self.parsed_bytes
    }

    /// Bytes of fenced code highlighted so far.
    #[cfg(test)]
    pub fn highlighted_bytes(&self) -> usize {
        self.highlighted_bytes
            + self
                .fence
                .as_ref()
                .map_or(0, |fence| fence.highlighted_bytes)
    }

    /// Render `source` and count the bytes handed to the parser.
    fn render(&mut self, range: std::ops::Range<usize>, preceded: bool) -> Vec<Line<'static>> {
        self.parsed_bytes += range.len();
        render_markdown_continuation(
            &self.buffer[range],
            self.width,
            self.cwd.as_deref(),
            preceded,
        )
    }

    /// Render everything pushed since the last call. Returns the lines that became final since
    /// then, oldest first, and refreshes the live tail.
    ///
    /// Rendering is append-only for the committed prefix except for markdown tables, whose column
    /// widths depend on every row: an unfinished trailing table is withheld (see
    /// [`committable_prefix_len`]) and shown live instead.
    pub fn advance(&mut self) -> Vec<Line<'static>> {
        if !self.dirty {
            return Vec::new();
        }
        self.dirty = false;
        let mut out = Vec::new();
        loop {
            if self.fence.is_none() && !self.open_fence(&mut out) {
                self.freeze_completed_blocks(&mut out);
                if !self.open_fence(&mut out) {
                    break;
                }
            }
            let Some(fence) = self.fence.as_mut() else {
                break;
            };
            match fence.advance(&self.buffer, &mut out) {
                fence::FenceProgress::Open => {
                    self.live = fence.live(&self.buffer);
                    return out;
                }
                fence::FenceProgress::Paused => {
                    // The rest of the burst is the next frames' work.
                    self.live.clear();
                    self.dirty = true;
                    return out;
                }
                fence::FenceProgress::Closed { end } => self.close_fence(end),
            }
        }

        let open_start = self.frozen_len;
        let preceded = self.prose_continues;
        let open_len = self.buffer.len() - open_start;
        let complete_len = match memchr::memrchr(b'\n', &self.buffer.as_bytes()[open_start..]) {
            Some(newline) => {
                committable_prefix_len(&self.buffer[open_start..=open_start + newline])
            }
            None => 0,
        };

        if complete_len > 0 && self.complete_cache.is_none_or(|len| len != complete_len) {
            let rendered = self.render(open_start..open_start + complete_len, preceded);
            let mut count = rendered.len();
            if count > 0 && is_blank_line_spaces_only(&rendered[count - 1]) {
                count -= 1;
            }
            if count > self.committed_in_open {
                out.extend(rendered[self.committed_in_open..count].iter().cloned());
                self.committed_in_open = count;
            }
            self.complete_cache = Some(complete_len);
        }

        // The live tail is the open block as it stands now, past what was committed. Nothing is
        // being typed when every byte of it is a committed line.
        let mut live: Vec<Line<'static>> = if complete_len == open_len {
            Vec::new()
        } else {
            self.render(open_start..self.buffer.len(), preceded)
                .into_iter()
                .skip(self.committed_in_open)
                .collect()
        };
        while live.last().is_some_and(is_blank_line_spaces_only) {
            live.pop();
        }
        // The rows of a long line that cannot change any more go to the scrollback now, once
        // everything before the line has.
        let line_start = open_start
            + memchr::memrchr(b'\n', &self.buffer.as_bytes()[open_start..])
                .map_or(0, |newline| newline + 1);
        if complete_len == line_start - open_start
            && let Some(width) = self.width
        {
            let committed_to = match self.line_committed {
                Some((start, to)) if start == line_start => to,
                Some(_) | None => line_start,
            };
            if let Some((rows, to)) = long_line::final_rows(
                &self.buffer,
                open_start,
                line_start,
                committed_to,
                &live,
                width,
            ) {
                out.extend(live.drain(..rows));
                self.committed_in_open += rows;
                self.line_committed = Some((line_start, to));
            }
        }
        self.live = live;
        out
    }

    /// Start streaming the open block as a fenced code block if it is one: its first line with
    /// content is a complete fence opener. The block's leading separator is rendered by the
    /// renderer, so it is exactly what rendering the whole message gives.
    fn open_fence(&mut self, out: &mut Vec<Line<'static>>) -> bool {
        let open = &self.buffer[self.frozen_len..];
        let mut offset = 0;
        for line in open.split_inclusive('\n') {
            let Some(content) = line.strip_suffix('\n') else {
                return false;
            };
            offset += line.len();
            if content.trim().is_empty() {
                continue;
            }
            let Some(opener) = FenceOpener::parse(content) else {
                return false;
            };
            let body_start = self.frozen_len + offset;
            let opening = self.render(self.frozen_len..body_start, self.prose_continues);
            out.extend(opening.into_iter().skip(self.committed_in_open));
            self.committed_in_open = 0;
            self.complete_cache = None;
            self.fence = Some(fence::OpenFence::new(opener, body_start));
            return true;
        }
        false
    }

    /// The open fence closed at `end`: the next block starts after it, preceded by it.
    fn close_fence(&mut self, end: usize) {
        if let Some(fence) = self.fence.take() {
            self.highlighted_bytes += fence.highlighted_bytes;
        }
        self.frozen_len = end;
        self.prose_continues = true;
        self.committed_in_open = 0;
        self.complete_cache = None;
        self.live.clear();
    }

    /// Freeze every block that a later block has closed: render it, hand out its lines, and move
    /// the open block's start past it.
    fn freeze_completed_blocks(&mut self, out: &mut Vec<Line<'static>>) {
        let tail = &self.buffer[self.frozen_len..];
        // Only complete lines decide block structure; the line being typed can still change it.
        let Some(stable_end) = memchr::memrchr(b'\n', tail.as_bytes()).map(|newline| newline + 1)
        else {
            return;
        };
        let stable = &tail[..stable_end];
        let segments = markdown_segments(stable);
        let Some(last) = segments.last() else {
            return;
        };

        // Where the open block starts, relative to the tail.
        let mut open_start = last.range.start;
        let mut open_continues_prose = false;
        if !last.is_table {
            let starts = top_level_block_starts(&stable[last.range.clone()]);
            self.parsed_bytes += last.range.len();
            if starts.len() >= 2 {
                // Freeze up to the open block, or up to the first fenced block before it: a fence
                // is streamed line by line instead, so a burst of code is highlighted over as
                // many frames as it takes rather than all in one.
                let open = (1..starts.len())
                    .find(|index| {
                        let block = &stable[last.range.start + starts[*index]..];
                        FenceOpener::parse(block.split('\n').next().unwrap_or_default()).is_some()
                    })
                    .unwrap_or(starts.len() - 1);
                open_start = last.range.start + starts[open];
                open_continues_prose = true;
            }
        }
        if open_start == 0 {
            return;
        }

        let from = self.frozen_len;
        let frozen = self.render(from..from + open_start, self.prose_continues);
        // A block that starts a segment of its own continues output when anything came before it.
        let continues = open_continues_prose || self.prose_continues || !frozen.is_empty();
        // The block that was open is the first thing frozen; its early lines were already
        // handed out as they were committed.
        out.extend(frozen.into_iter().skip(self.committed_in_open));
        self.frozen_len += open_start;
        self.prose_continues = continues;
        self.committed_in_open = 0;
        self.complete_cache = None;
    }

    /// Finalize the stream: hand out everything still pending. If the buffer does not end with a
    /// newline, a temporary one is appended for rendering. Returns the raw source unchanged.
    pub fn finalize_and_drain(&mut self) -> (Vec<Line<'static>>, String) {
        self.dirty = true;
        let mut out = Vec::new();
        while self.dirty {
            out.extend(self.advance());
        }
        if let Some(fence) = self.fence.as_mut() {
            // The message ended inside a code block, which ends it.
            fence.finish(&self.buffer, &mut out);
            let end = self.buffer.len();
            self.close_fence(end);
        }

        let mut appended_newline = false;
        if self.frozen_len < self.buffer.len() {
            if !self.buffer.ends_with('\n') {
                self.buffer.push('\n');
                appended_newline = true;
            }
            tracing::debug!(raw_len = self.buffer.len(), "markdown finalize");
            let rendered = self.render(self.frozen_len..self.buffer.len(), self.prose_continues);
            out.extend(rendered.into_iter().skip(self.committed_in_open));
        }

        let mut raw = std::mem::take(&mut self.buffer);
        // The temporary newline is not part of the message.
        if appended_newline {
            raw.pop();
        }
        self.clear();
        (out, raw)
    }

    /// Advance and return only the newly committed lines. A convenience for tests that commit
    /// after each delta.
    #[cfg(test)]
    pub fn commit_complete_lines(&mut self) -> Vec<Line<'static>> {
        self.dirty = true;
        self.advance()
    }
}

#[cfg(test)]
mod incremental_tests;

#[cfg(test)]
pub(crate) fn simulate_stream_markdown_for_tests(
    deltas: &[&str],
    finalize: bool,
) -> Vec<Line<'static>> {
    let mut collector = MarkdownStreamCollector::new(None);
    let mut out = Vec::new();
    for d in deltas {
        collector.push_delta(d);
        if d.contains('\n') {
            out.extend(collector.commit_complete_lines());
        }
    }
    if finalize {
        out.extend(collector.finalize_and_drain().0);
    }
    out
}
#[cfg(test)]
mod tests;
