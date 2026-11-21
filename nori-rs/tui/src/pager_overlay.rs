use std::io::Result;
use std::sync::Arc;
use std::time::Duration;

use crate::history_cell::HistoryCell;
use crate::key_hint;
use crate::key_hint::KeyBinding;
use crate::render::renderable::Renderable;
use crate::tui;
use crate::tui::TuiEvent;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use ratatui::buffer::Buffer;
use ratatui::buffer::Cell;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use ratatui::widgets::Wrap;

mod message_menu;
mod transcript;
#[cfg(test)]
mod transcript_tests;

pub(crate) use message_menu::MessageAction;
pub(crate) use message_menu::MessageChoice;
pub(crate) use transcript::TranscriptOverlay;

pub(crate) enum Overlay {
    Transcript(TranscriptOverlay),
    Static(StaticOverlay),
    /// The subagent switcher.
    Subagents(Box<crate::subagent_switcher::SubagentOverlay>),
    /// Helix-inspired two-pane `/resume` session picker.
    Resume(Box<crate::resume_picker::helix_overlay::ResumeOverlay>),
    /// The `/usage` view.
    Usage(Box<crate::usage::UsageOverlay>),
    /// A list of choices, as data: see [`crate::choice_picker`].
    Choice(Box<crate::choice_picker::ChoiceOverlay>),
}

impl Overlay {
    pub(crate) fn new_transcript(cells: Vec<Arc<dyn HistoryCell>>) -> Self {
        Self::Transcript(TranscriptOverlay::new(cells))
    }

    pub(crate) fn new_static_with_lines(lines: Vec<Line<'static>>, title: String) -> Self {
        Self::Static(StaticOverlay::with_title(lines, title))
    }

    pub(crate) fn new_static_with_renderables(
        renderables: Vec<Box<dyn Renderable>>,
        title: String,
    ) -> Self {
        Self::Static(StaticOverlay::with_renderables(renderables, title))
    }

    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match self {
            Overlay::Transcript(o) => o.handle_event(tui, event),
            Overlay::Static(o) => o.handle_event(tui, event),
            Overlay::Subagents(o) => o.handle_event(tui, event),
            Overlay::Resume(o) => o.handle_event(tui, event),
            Overlay::Usage(o) => o.handle_event(tui, event),
            Overlay::Choice(o) => o.handle_event(tui, event),
        }
    }

    pub(crate) fn is_done(&self) -> bool {
        match self {
            Overlay::Transcript(o) => o.is_done(),
            Overlay::Static(o) => o.is_done(),
            Overlay::Subagents(o) => o.is_done(),
            Overlay::Resume(o) => o.is_done(),
            Overlay::Usage(o) => o.is_done(),
            Overlay::Choice(o) => o.is_done(),
        }
    }
}

const KEY_UP: KeyBinding = key_hint::plain(KeyCode::Up);
const KEY_DOWN: KeyBinding = key_hint::plain(KeyCode::Down);
const KEY_PAGE_UP: KeyBinding = key_hint::plain(KeyCode::PageUp);
const KEY_PAGE_DOWN: KeyBinding = key_hint::plain(KeyCode::PageDown);
const KEY_SPACE: KeyBinding = key_hint::plain(KeyCode::Char(' '));
const KEY_HOME: KeyBinding = key_hint::plain(KeyCode::Home);
const KEY_END: KeyBinding = key_hint::plain(KeyCode::End);
const KEY_Q: KeyBinding = key_hint::plain(KeyCode::Char('q'));
const KEY_ESC: KeyBinding = key_hint::plain(KeyCode::Esc);
const KEY_ENTER: KeyBinding = key_hint::plain(KeyCode::Enter);
const KEY_CTRL_T: KeyBinding = key_hint::ctrl(KeyCode::Char('t'));
const KEY_CTRL_C: KeyBinding = key_hint::ctrl(KeyCode::Char('c'));
const TRANSCRIPT_HISTORY_MAX_LINES: usize = 20;

/// Where each item of a pager starts, in rows, at one width.
///
/// Prefix sums of the item heights. They make "which item is at this row" a binary search and
/// "append an item" a single push, so neither scrolling nor adding a cell depends on how many
/// cells the transcript holds.
struct RowIndex {
    width: u16,
    /// `tops[i]` is the first row of item `i`; the last entry is the total number of rows.
    tops: Vec<usize>,
}

impl RowIndex {
    fn new(width: u16) -> Self {
        Self {
            width,
            tops: vec![0],
        }
    }

    fn items(&self) -> usize {
        self.tops.len() - 1
    }

    fn total(&self) -> usize {
        self.tops.last().copied().unwrap_or(0)
    }

    fn push(&mut self, height: usize) {
        self.tops.push(self.total() + height);
    }

    fn truncate(&mut self, items: usize) {
        self.tops.truncate(items + 1);
    }

    fn top_of(&self, item: usize) -> usize {
        self.tops[item]
    }

    /// The item that covers `row`, or the item count when `row` is past the end.
    fn item_at_row(&self, row: usize) -> usize {
        self.tops
            .partition_point(|top| *top <= row)
            .saturating_sub(1)
            .min(self.items())
    }
}

/// Generic widget for rendering a pager view.
struct PagerView {
    renderables: Vec<Box<dyn Renderable>>,
    /// Row layout of `renderables`; rebuilt only when the width changes.
    index: Option<RowIndex>,
    scroll_offset: usize,
    title: String,
    last_content_height: Option<usize>,
    last_rendered_height: Option<usize>,
    /// If set, on next render ensure this chunk is visible.
    pending_scroll_chunk: Option<usize>,
    /// Where the last frame drew the items, to find the item under a click.
    last_content_area: Option<Rect>,
}

impl PagerView {
    fn new(renderables: Vec<Box<dyn Renderable>>, title: String, scroll_offset: usize) -> Self {
        Self {
            renderables,
            index: None,
            scroll_offset,
            title,
            last_content_height: None,
            last_rendered_height: None,
            pending_scroll_chunk: None,
            last_content_area: None,
        }
    }

    /// The row layout at `width`, laying items out only if the width changed.
    fn index_for(&mut self, width: u16) -> &RowIndex {
        let valid = self
            .index
            .as_ref()
            .is_some_and(|index| index.width == width && index.items() == self.renderables.len());
        if !valid {
            let mut index = RowIndex::new(width);
            for renderable in &self.renderables {
                index.push(usize::from(renderable.desired_height(width)));
            }
            self.index = Some(index);
        }
        self.index.get_or_insert_with(|| RowIndex::new(width))
    }

    fn content_height(&mut self, width: u16) -> usize {
        self.index_for(width).total()
    }

    /// Add an item at the end. Costs the rows of that item.
    fn push_renderable(&mut self, renderable: Box<dyn Renderable>) {
        if let Some(index) = self.index.as_mut() {
            index.push(usize::from(renderable.desired_height(index.width)));
        }
        self.renderables.push(renderable);
    }

    /// Replace the items from `start` on with `replacement`, keeping the layout of those before
    /// `start`.
    fn replace_from(&mut self, start: usize, replacement: Vec<Box<dyn Renderable>>) {
        self.renderables.truncate(start);
        if let Some(index) = self.index.as_mut() {
            index.truncate(start);
        }
        for renderable in replacement {
            self.push_renderable(renderable);
        }
    }

    fn render(&mut self, area: Rect, buf: &mut Buffer) {
        Clear.render(area, buf);
        self.render_header(area, buf);
        let content_area = self.content_area(area);
        self.update_last_content_height(content_area.height);
        let content_height = self.content_height(content_area.width);
        self.last_rendered_height = Some(content_height);
        // If there is a pending request to scroll a specific chunk into view,
        // satisfy it now that wrapping is up to date for this width.
        if let Some(idx) = self.pending_scroll_chunk.take() {
            self.ensure_chunk_visible(idx, content_area);
        }
        self.scroll_offset = self
            .scroll_offset
            .min(content_height.saturating_sub(content_area.height as usize));
        self.last_content_area = Some(content_area);

        self.render_content(content_area, buf);

        self.render_bottom_bar(area, content_area, buf, content_height);
    }

    fn render_header(&self, area: Rect, buf: &mut Buffer) {
        Span::from("/ ".repeat(area.width as usize / 2))
            .dim()
            .render(area, buf);
        let header = format!("/ {}", self.title);
        header.dim().render(area, buf);
    }

    fn render_content(&self, area: Rect, buf: &mut Buffer) {
        let mut drawn_bottom = area.y;
        if let Some(index) = self
            .index
            .as_ref()
            .filter(|index| index.width == area.width)
        {
            // Only the items that intersect the viewport are touched.
            let first = index.item_at_row(self.scroll_offset);
            for item in first..self.renderables.len() {
                let top = index.top_of(item) as isize - self.scroll_offset as isize;
                if top > area.height as isize {
                    break;
                }
                let renderable = &self.renderables[item];
                let height = (index.top_of(item + 1) - index.top_of(item)) as isize;
                if top + height < 0 {
                    continue;
                }
                if top < 0 {
                    let drawn = render_offset_content(area, buf, &**renderable, (-top) as u16);
                    drawn_bottom = drawn_bottom.max(area.y + drawn);
                } else {
                    let draw_height = (height as u16).min(area.height.saturating_sub(top as u16));
                    let draw_area = Rect::new(area.x, area.y + top as u16, area.width, draw_height);
                    renderable.render(draw_area, buf);
                    drawn_bottom = drawn_bottom.max(draw_area.y.saturating_add(draw_area.height));
                }
            }
        }

        for y in drawn_bottom..area.bottom() {
            if area.width == 0 {
                break;
            }
            buf[(area.x, y)] = Cell::from('~');
            for x in area.x + 1..area.right() {
                buf[(x, y)] = Cell::from(' ');
            }
        }
    }

    fn render_bottom_bar(
        &self,
        full_area: Rect,
        content_area: Rect,
        buf: &mut Buffer,
        total_len: usize,
    ) {
        let sep_y = content_area.bottom();
        let sep_rect = Rect::new(full_area.x, sep_y, full_area.width, 1);

        Span::from("─".repeat(sep_rect.width as usize))
            .dim()
            .render(sep_rect, buf);
        let percent = if total_len == 0 {
            100
        } else {
            let max_scroll = total_len.saturating_sub(content_area.height as usize);
            if max_scroll == 0 {
                100
            } else {
                (((self.scroll_offset.min(max_scroll)) as f32 / max_scroll as f32) * 100.0).round()
                    as u8
            }
        };
        let pct_text = format!(" {percent}% ");
        let pct_w = pct_text.chars().count() as u16;
        let pct_x = sep_rect.x + sep_rect.width - pct_w - 1;
        Span::from(pct_text)
            .dim()
            .render(Rect::new(pct_x, sep_rect.y, pct_w, 1), buf);
    }

    fn handle_key_event(&mut self, tui: &mut tui::Tui, key_event: KeyEvent) -> Result<()> {
        match key_event {
            e if KEY_UP.is_press(e) => {
                self.scroll_offset = self.scroll_offset.saturating_sub(1);
            }
            e if KEY_DOWN.is_press(e) => {
                self.scroll_offset = self.scroll_offset.saturating_add(1);
            }
            e if KEY_PAGE_UP.is_press(e) => {
                let area = self.content_area(tui.terminal.viewport_area);
                self.scroll_offset = self.scroll_offset.saturating_sub(area.height as usize);
            }
            e if KEY_PAGE_DOWN.is_press(e) || KEY_SPACE.is_press(e) => {
                let area = self.content_area(tui.terminal.viewport_area);
                self.scroll_offset = self.scroll_offset.saturating_add(area.height as usize);
            }
            e if KEY_HOME.is_press(e) => {
                self.scroll_offset = 0;
            }
            e if KEY_END.is_press(e) => {
                self.scroll_offset = usize::MAX;
            }
            _ => {
                return Ok(());
            }
        }
        tui.frame_requester()
            .schedule_frame_in(Duration::from_millis(16));
        Ok(())
    }

    fn update_last_content_height(&mut self, height: u16) {
        self.last_content_height = Some(height as usize);
    }

    fn content_area(&self, area: Rect) -> Rect {
        let mut area = area;
        area.y = area.y.saturating_add(1);
        area.height = area.height.saturating_sub(2);
        area
    }
}

impl PagerView {
    /// The item the last frame drew at screen row `y`, if any.
    fn item_at(&self, y: u16) -> Option<usize> {
        let area = self.last_content_area?;
        if y < area.y || y >= area.bottom() {
            return None;
        }
        let index = self
            .index
            .as_ref()
            .filter(|index| index.width == area.width)?;
        let row = self.scroll_offset.saturating_add(usize::from(y - area.y));
        (row < index.total()).then(|| index.item_at_row(row))
    }

    /// The screen rows the last frame drew `item` on, top and bottom (exclusive), cut to the
    /// content area.
    fn item_rows(&self, item: usize) -> Option<(u16, u16)> {
        let area = self.last_content_area?;
        let index = self
            .index
            .as_ref()
            .filter(|index| index.width == area.width && item < index.items())?;
        let on_screen = |row: usize| {
            let row = row
                .saturating_sub(self.scroll_offset)
                .min(usize::from(area.height));
            area.y + u16::try_from(row).unwrap_or(area.height)
        };
        Some((
            on_screen(index.top_of(item)),
            on_screen(index.top_of(item + 1)),
        ))
    }

    fn is_scrolled_to_bottom(&self) -> bool {
        if self.scroll_offset == usize::MAX {
            return true;
        }
        let Some(height) = self.last_content_height else {
            return false;
        };
        if self.renderables.is_empty() {
            return true;
        }
        let Some(total_height) = self.last_rendered_height else {
            return false;
        };
        if total_height <= height {
            return true;
        }
        let max_scroll = total_height.saturating_sub(height);
        self.scroll_offset >= max_scroll
    }

    /// Request that the given text chunk index be scrolled into view on next render.
    fn scroll_chunk_into_view(&mut self, chunk_index: usize) {
        self.pending_scroll_chunk = Some(chunk_index);
    }

    fn ensure_chunk_visible(&mut self, idx: usize, area: Rect) {
        if area.height == 0 || idx >= self.renderables.len() {
            return;
        }
        let index = self.index_for(area.width);
        let first = index.top_of(idx);
        let last = index.top_of(idx + 1);
        let current_top = self.scroll_offset;
        let current_bottom = current_top.saturating_add(area.height.saturating_sub(1) as usize);
        if first < current_top {
            self.scroll_offset = first;
        } else if last > current_bottom {
            self.scroll_offset = last.saturating_sub(area.height.saturating_sub(1) as usize);
        }
    }
}

/// A renderable that caches its desired height.
struct CachedRenderable {
    renderable: Box<dyn Renderable>,
    height: std::cell::Cell<Option<u16>>,
    last_width: std::cell::Cell<Option<u16>>,
}

impl CachedRenderable {
    fn new(renderable: impl Into<Box<dyn Renderable>>) -> Self {
        Self {
            renderable: renderable.into(),
            height: std::cell::Cell::new(None),
            last_width: std::cell::Cell::new(None),
        }
    }
}

impl Renderable for CachedRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.renderable.render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        if self.last_width.get() != Some(width) {
            let height = self.renderable.desired_height(width);
            self.height.set(Some(height));
            self.last_width.set(Some(width));
        }
        self.height.get().unwrap_or(0)
    }
}

struct CellRenderable {
    cell: Arc<dyn HistoryCell>,
    style: Style,
}

pub(crate) fn transcript_overlay_lines(cell: &dyn HistoryCell, width: u16) -> Vec<Line<'static>> {
    let mut lines = cell.transcript_lines(width);
    if cell.full_transcript() || lines.len() <= TRANSCRIPT_HISTORY_MAX_LINES {
        return lines;
    }

    let omitted = lines.len() - (TRANSCRIPT_HISTORY_MAX_LINES - 1);
    lines.truncate(TRANSCRIPT_HISTORY_MAX_LINES - 1);
    lines.push(Line::from(format!("… {omitted} more lines")).dim());
    lines
}

fn transcript_overlay_height(cell: &dyn HistoryCell, width: u16) -> u16 {
    transcript_overlay_lines(cell, width).len() as u16
}

impl Renderable for CellRenderable {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let p = Paragraph::new(Text::from(transcript_overlay_lines(
            self.cell.as_ref(),
            area.width,
        )))
        .style(self.style);
        p.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        transcript_overlay_height(self.cell.as_ref(), width)
    }
}

pub(crate) struct StaticOverlay {
    view: PagerView,
    is_done: bool,
}

impl StaticOverlay {
    pub(crate) fn with_title(lines: Vec<Line<'static>>, title: String) -> Self {
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        Self::with_renderables(vec![Box::new(CachedRenderable::new(paragraph))], title)
    }

    pub(crate) fn with_renderables(renderables: Vec<Box<dyn Renderable>>, title: String) -> Self {
        Self {
            view: PagerView::new(renderables, title, 0),
            is_done: false,
        }
    }

    pub(crate) fn render(&mut self, area: Rect, buf: &mut Buffer) {
        let top_h = area.height.saturating_sub(1);
        let top = Rect::new(area.x, area.y, area.width, top_h);
        let bottom = Rect::new(area.x, area.y + top_h, area.width, 1);
        self.view.render(top, buf);
        crate::bottom_pane::render_mode_row(bottom, buf, crate::bottom_pane::StatusMode::View);
    }
}

impl StaticOverlay {
    pub(crate) fn handle_event(&mut self, tui: &mut tui::Tui, event: TuiEvent) -> Result<()> {
        match event {
            TuiEvent::Key(key_event) => match key_event {
                e if KEY_Q.is_press(e) || KEY_CTRL_C.is_press(e) => {
                    self.is_done = true;
                    Ok(())
                }
                other => self.view.handle_key_event(tui, other),
            },
            TuiEvent::Draw => {
                tui.draw_overlay(|frame| {
                    self.render(frame.area(), frame.buffer);
                })?;
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub(crate) fn is_done(&self) -> bool {
        self.is_done
    }
}

fn render_offset_content(
    area: Rect,
    buf: &mut Buffer,
    renderable: &dyn Renderable,
    scroll_offset: u16,
) -> u16 {
    let height = renderable.desired_height(area.width);
    let mut tall_buf = Buffer::empty(Rect::new(
        0,
        0,
        area.width,
        height.min(area.height + scroll_offset),
    ));
    renderable.render(*tall_buf.area(), &mut tall_buf);
    let copy_height = area
        .height
        .min(tall_buf.area().height.saturating_sub(scroll_offset));
    for y in 0..copy_height {
        let src_y = y + scroll_offset;
        for x in 0..area.width {
            buf[(area.x + x, area.y + y)] = tall_buf[(x, src_y)].clone();
        }
    }

    copy_height
}

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_snapshot;
    use std::sync::Arc;

    use crate::history_cell::HistoryCell;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::text::Text;

    #[derive(Debug)]
    struct TestCell {
        lines: Vec<Line<'static>>,
    }

    impl crate::history_cell::HistoryCell for TestCell {
        fn display_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }

        fn transcript_lines(&self, _width: u16) -> Vec<Line<'static>> {
            self.lines.clone()
        }
    }

    fn paragraph_block(label: &str, lines: usize) -> Box<dyn Renderable> {
        let text = Text::from(
            (0..lines)
                .map(|i| Line::from(format!("{label}{i}")))
                .collect::<Vec<_>>(),
        );
        Box::new(Paragraph::new(text)) as Box<dyn Renderable>
    }

    #[test]
    fn transcript_footer_shows_its_mode_badge() {
        let mut overlay = TranscriptOverlay::new(vec![Arc::new(TestCell {
            lines: vec![Line::from("hello")],
        })]);

        // Render into a small buffer and assert the transcript mode badge is present
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        overlay.render(area, &mut buf);

        // Flatten buffer to a string and check for the hint text
        let mut s = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                s.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
            s.push('\n');
        }
        assert!(
            s.contains(" TRANSCRIPT "),
            "expected the transcript mode badge in the overlay footer, got: {s:?}"
        );
    }

    #[test]
    fn transcript_overlay_snapshot_basic() {
        // Prepare a transcript overlay with a few lines
        let mut overlay = TranscriptOverlay::new(vec![
            Arc::new(TestCell {
                lines: vec![Line::from("alpha")],
            }),
            Arc::new(TestCell {
                lines: vec![Line::from("beta")],
            }),
            Arc::new(TestCell {
                lines: vec![Line::from("gamma")],
            }),
        ]);
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn transcript_overlay_truncates_non_patch_cells_to_twenty_lines() {
        let mut overlay = TranscriptOverlay::new(vec![Arc::new(TestCell {
            lines: (0..25)
                .map(|i| Line::from(format!("non-patch-row-{i:02}")))
                .collect(),
        })]);
        let area = Rect::new(0, 0, 80, 30);
        let mut buf = Buffer::empty(area);

        overlay.render(area, &mut buf);

        let rendered = buffer_to_text(&buf, area);
        assert!(rendered.contains("non-patch-row-18"));
        assert!(!rendered.contains("non-patch-row-19"));
        assert!(rendered.contains("… 6 more lines"));
    }

    fn buffer_to_text(buf: &Buffer, area: Rect) -> String {
        let mut out = String::new();
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                let symbol = buf[(x, y)].symbol();
                if symbol.is_empty() {
                    out.push(' ');
                } else {
                    out.push(symbol.chars().next().unwrap_or(' '));
                }
            }
            // Trim trailing spaces for stability.
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn transcript_overlay_keeps_scroll_pinned_at_bottom() {
        let mut overlay = TranscriptOverlay::new(
            (0..20)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");

        assert!(
            overlay.view.is_scrolled_to_bottom(),
            "expected initial render to leave view at bottom"
        );

        overlay.insert_cell(Arc::new(TestCell {
            lines: vec!["tail".into()],
        }));

        assert_eq!(overlay.view.scroll_offset, usize::MAX);
    }

    #[test]
    fn transcript_overlay_preserves_manual_scroll_position() {
        let mut overlay = TranscriptOverlay::new(
            (0..20)
                .map(|i| {
                    Arc::new(TestCell {
                        lines: vec![Line::from(format!("line{i}"))],
                    }) as Arc<dyn HistoryCell>
                })
                .collect(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 12)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");

        overlay.view.scroll_offset = 0;

        overlay.insert_cell(Arc::new(TestCell {
            lines: vec!["tail".into()],
        }));

        assert_eq!(overlay.view.scroll_offset, 0);
    }

    #[test]
    fn transcript_overlay_can_replace_finalized_stream_cells() {
        let mut overlay = TranscriptOverlay::new(vec![
            Arc::new(TestCell {
                lines: vec![Line::from("tool")],
            }),
            Arc::new(crate::history_cell::AgentMessageCell::new(
                vec![Line::from("first")],
                true,
            )),
            Arc::new(crate::history_cell::AgentMessageCell::new(
                vec![Line::from("second")],
                false,
            )),
        ]);
        let replacement: Arc<dyn HistoryCell> =
            Arc::new(crate::history_cell::AgentMarkdownCell::new(
                "first\nsecond".to_string(),
                std::path::Path::new("/tmp"),
            ));

        overlay.replace_cells(1..3, replacement);

        assert_eq!(overlay.cells.len(), 2);
        assert!(
            overlay.cells[1]
                .as_any()
                .is::<crate::history_cell::AgentMarkdownCell>()
        );
    }

    #[test]
    fn static_overlay_snapshot_basic() {
        // Prepare a static overlay with a few lines and a title
        let mut overlay = StaticOverlay::with_title(
            vec!["one".into(), "two".into(), "three".into()],
            "S T A T I C".to_string(),
        );
        let mut term = Terminal::new(TestBackend::new(40, 10)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn static_overlay_wraps_long_lines() {
        let mut overlay = StaticOverlay::with_title(
            vec!["a very long line that should wrap when rendered within a narrow pager overlay width".into()],
            "S T A T I C".to_string(),
        );
        let mut term = Terminal::new(TestBackend::new(24, 8)).expect("term");
        term.draw(|f| overlay.render(f.area(), f.buffer_mut()))
            .expect("draw");
        assert_snapshot!(term.backend());
    }

    #[test]
    fn pager_view_content_height_counts_renderables() {
        let mut pv = PagerView::new(
            vec![paragraph_block("a", 2), paragraph_block("b", 3)],
            "T".to_string(),
            0,
        );

        assert_eq!(pv.content_height(80), 5);
    }

    #[test]
    fn pager_view_ensure_chunk_visible_scrolls_down_when_needed() {
        let mut pv = PagerView::new(
            vec![
                paragraph_block("a", 1),
                paragraph_block("b", 3),
                paragraph_block("c", 3),
            ],
            "T".to_string(),
            0,
        );
        let area = Rect::new(0, 0, 20, 8);

        pv.scroll_offset = 0;
        let content_area = pv.content_area(area);
        pv.ensure_chunk_visible(2, content_area);

        let mut buf = Buffer::empty(area);
        pv.render(area, &mut buf);
        let rendered = buffer_to_text(&buf, area);

        assert!(
            rendered.contains("c0"),
            "expected chunk top in view: {rendered:?}"
        );
        assert!(
            rendered.contains("c1"),
            "expected chunk middle in view: {rendered:?}"
        );
        assert!(
            rendered.contains("c2"),
            "expected chunk bottom in view: {rendered:?}"
        );
    }

    #[test]
    fn pager_view_ensure_chunk_visible_scrolls_up_when_needed() {
        let mut pv = PagerView::new(
            vec![
                paragraph_block("a", 2),
                paragraph_block("b", 3),
                paragraph_block("c", 3),
            ],
            "T".to_string(),
            0,
        );
        let area = Rect::new(0, 0, 20, 3);

        pv.scroll_offset = 6;
        pv.ensure_chunk_visible(0, area);

        assert_eq!(pv.scroll_offset, 0);
    }

    #[test]
    fn pager_view_is_scrolled_to_bottom_accounts_for_wrapped_height() {
        let mut pv = PagerView::new(vec![paragraph_block("a", 10)], "T".to_string(), 0);
        let area = Rect::new(0, 0, 20, 8);
        let mut buf = Buffer::empty(area);

        pv.render(area, &mut buf);

        assert!(
            !pv.is_scrolled_to_bottom(),
            "expected view to report not at bottom when offset < max"
        );

        pv.scroll_offset = usize::MAX;
        pv.render(area, &mut buf);

        assert!(
            pv.is_scrolled_to_bottom(),
            "expected view to report at bottom after scrolling to end"
        );
    }
}
