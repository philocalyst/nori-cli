//! Drawing a [`HelixState`]: the frame, the prompt, the list and the preview.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use super::frame::FrameLayout;
use super::frame::PANE_PAD;
use super::frame::PREVIEW_MIN_WIDTH;
use super::frame::draw_frame;
use super::highlight::highlight_terms;
use super::highlight::line_matches;
use super::item::HelixItem;
use super::item::RowContext;
use super::preview::PreviewAnchor;
use super::state::HelixState;
use crate::Theme;

/// The longest name shown beside the verb of a compose prompt.
const COMPOSE_TARGET_MAX: usize = 16;

/// `name` cut to [`COMPOSE_TARGET_MAX`] characters with an ellipsis.
fn short(name: &str) -> String {
    if name.chars().count() <= COMPOSE_TARGET_MAX {
        return name.to_owned();
    }
    let cut: String = name.chars().take(COMPOSE_TARGET_MAX - 1).collect();
    format!("{cut}…")
}

/// Columns the selection marker and its space take at the start of a list row.
const MARKER_WIDTH: u16 = 3;
/// Columns the multi-select gutter (`◼ `) adds.
const GUTTER_WIDTH: u16 = 2;
/// Rows the box takes beside its list: the top border, the prompt, the rule under it and the
/// bottom border.
const CHROME_ROWS: u16 = 4;
/// The fewest body rows a preview is given, so that it shows something when the list is short.
const PREVIEW_MIN_ROWS: u16 = 8;

impl<I: HelixItem> HelixState<I> {
    /// Draw the picker into `area`. `tick` is passed through to rows for animation.
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme, tick: i64) {
        Clear.render(area, buf);
        self.refresh();
        let layout = FrameLayout::new(area, self.config.preview && !self.preview_hidden);
        self.layout = Some(layout);
        self.drawn.clear();
        if area.width < super::frame::MIN_WIDTH || area.height < super::frame::MIN_HEIGHT {
            return;
        }
        let trailing = self
            .search_time
            .map(|elapsed| format!("{:.1}ms", elapsed.as_secs_f64() * 1000.0))
            .unwrap_or_default();
        draw_frame(
            &layout,
            buf,
            theme.border,
            (
                &self.config.title,
                theme.accent_cyan.add_modifier(Modifier::BOLD),
            ),
            self.status
                .as_deref()
                .or(self.config.subtitle.as_deref())
                .unwrap_or_default(),
            &trailing,
        );
        self.render_prompt(layout.prompt, buf, theme);
        self.render_list(layout.list, buf, theme, tick);
        if let (Some(title), Some(pane)) = (layout.preview_title, layout.preview) {
            self.render_preview(title, pane, buf, theme);
        }
    }

    /// The height of the box that shows every row at `width`: the rows of the list, or the
    /// preview's least when a preview is drawn beside them, plus the chrome around them. A host
    /// that draws the box at this height leaves no empty rows under a short list.
    pub fn height_wanted(&mut self, width: u16) -> u16 {
        self.refresh();
        let filtered = !self.parsed.is_empty();
        let mut list: u16 = 0;
        let mut previous: Option<&str> = None;
        for found in &self.matches {
            let item = &self.items[found.index];
            let section = item.section();
            // A heading is drawn above each run of a section while the list is in natural order.
            if !filtered && section.is_some() && section != previous {
                list = list.saturating_add(1);
            }
            previous = section;
            list = list.saturating_add(item.row_height().max(1));
        }
        let shows_preview =
            self.config.preview && !self.preview_hidden && width >= PREVIEW_MIN_WIDTH;
        let body = if shows_preview {
            list.max(PREVIEW_MIN_ROWS)
        } else {
            list
        };
        body.max(1).saturating_add(CHROME_ROWS)
    }

    fn render_prompt(&self, area: Rect, buf: &mut Buffer, theme: &Theme) {
        if let Some(compose) = &self.compose {
            let mut spans: Vec<Span<'static>> = vec![
                " ".into(),
                Span::styled("✉ ", theme.accent_amber),
                Span::styled(
                    format!("{} {}", compose.verb, short(&compose.target)),
                    theme.accent_amber.add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ❯ ", theme.accent_mint.add_modifier(Modifier::BOLD)),
            ];
            // Show the end of what is being typed.
            let room = usize::from(area.width)
                .saturating_sub(spans.iter().map(Span::width).sum::<usize>() + 2);
            let shown: String = if compose.masked {
                "•".repeat(compose.text.chars().count())
            } else {
                compose.text.clone()
            };
            let tail: String = shown
                .chars()
                .rev()
                .scan(0, |used, character| {
                    *used += UnicodeWidthChar::width(character).unwrap_or(0);
                    (*used <= room).then_some(character)
                })
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            spans.push(Span::raw(tail));
            spans.push(Span::styled(
                " ",
                Style::new().add_modifier(Modifier::REVERSED),
            ));
            buf.set_line(area.x, area.y, &Line::from(spans), area.width);
            return;
        }
        if !self.config.searchable {
            let line = Line::from(vec![
                " ".into(),
                Span::styled(self.config.placeholder.clone(), theme.faint),
            ]);
            buf.set_line(area.x, area.y, &line, area.width);
            return;
        }
        let count = format!("{} / {}", self.matches.len(), self.items.len());
        let count_width = UnicodeWidthStr::width(count.as_str());
        // One column of air, the glyph and its space, the cursor, the count and a gap.
        let room = usize::from(area.width).saturating_sub(count_width + 6);
        let mut spans: Vec<Span<'static>> = vec![
            " ".into(),
            Span::styled("❯ ", theme.accent_mint.add_modifier(Modifier::BOLD)),
        ];
        if self.query.is_empty() {
            spans.push(Span::styled(
                " ",
                Style::new().add_modifier(Modifier::REVERSED),
            ));
            // The placeholder shows whole or not at all, never cut against the count.
            let placeholder = format!(" {}", self.config.placeholder);
            if UnicodeWidthStr::width(placeholder.as_str()) <= room {
                spans.push(Span::styled(placeholder, theme.faint));
            }
        } else {
            spans.extend(self.query_spans(room, theme));
            spans.push(Span::styled(
                " ",
                Style::new().add_modifier(Modifier::REVERSED),
            ));
        }
        let left_room = area
            .width
            .saturating_sub(u16::try_from(count_width).unwrap_or(0) + 2);
        buf.set_line(area.x, area.y, &Line::from(spans), left_room);
        let at = area
            .right()
            .saturating_sub(u16::try_from(count_width).unwrap_or(0) + 1);
        if at > area.x {
            buf.set_stringn(at, area.y, &count, count_width, theme.faint);
        }
    }

    /// The query in `room` columns: filters styled apart from text, the start cut with an
    /// ellipsis when the end is what the user is typing.
    fn query_spans(&self, room: usize, theme: &Theme) -> Vec<Span<'static>> {
        let filter_style = theme.accent_violet.add_modifier(Modifier::ITALIC);
        let styled: Vec<(char, bool)> = self
            .query
            .char_indices()
            .map(|(offset, character)| {
                let in_filter = self
                    .parsed
                    .filter_spans
                    .iter()
                    .any(|span| span.contains(&offset));
                (character, in_filter)
            })
            .collect();
        let width = |characters: &[(char, bool)]| -> usize {
            characters
                .iter()
                .map(|(character, _)| UnicodeWidthChar::width(*character).unwrap_or(0))
                .sum()
        };
        let mut skip = 0;
        while width(&styled[skip..]) > room.saturating_sub(usize::from(skip > 0))
            && skip < styled.len()
        {
            skip += 1;
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        if skip > 0 {
            spans.push(Span::styled("…", theme.faint));
        }
        let mut run = String::new();
        let mut run_filter = false;
        for (character, in_filter) in &styled[skip..] {
            if *in_filter != run_filter && !run.is_empty() {
                let style = if run_filter {
                    filter_style
                } else {
                    Style::new()
                };
                spans.push(Span::styled(std::mem::take(&mut run), style));
            }
            run_filter = *in_filter;
            run.push(*character);
        }
        if !run.is_empty() {
            spans.push(Span::styled(
                run,
                if run_filter {
                    filter_style
                } else {
                    Style::new()
                },
            ));
        }
        spans
    }

    fn render_list(&mut self, area: Rect, buf: &mut Buffer, theme: &Theme, tick: i64) {
        if area.is_empty() {
            return;
        }
        if self.matches.is_empty() {
            let text = if self.query.is_empty() {
                &self.config.empty_text
            } else {
                &self.config.no_match_text
            };
            let room = usize::from(area.width)
                .saturating_sub(usize::from(MARKER_WIDTH) + 1)
                .max(1);
            for (row, wrapped) in textwrap::wrap(text, room)
                .iter()
                .take(usize::from(area.height))
                .enumerate()
            {
                let y = area.y + u16::try_from(row).unwrap_or(0);
                buf.set_stringn(
                    area.x + MARKER_WIDTH,
                    y,
                    wrapped,
                    room,
                    theme.faint.add_modifier(Modifier::ITALIC),
                );
            }
            return;
        }
        let filtered = !self.parsed.is_empty();
        // The heading a row opens a section with: shown while the list is in natural order,
        // and always above the first visible row so a scrolled list says where it is.
        let heading = |state: &Self, position: usize, first: bool| -> Option<String> {
            if filtered {
                return None;
            }
            let section = state.items[state.matches[position].index].section()?;
            let previous = position
                .checked_sub(1)
                .and_then(|before| state.items[state.matches[before].index].section());
            (first || previous != Some(section)).then(|| section.to_owned())
        };
        let height = |state: &Self, position: usize, first: bool| -> u16 {
            state.items[state.matches[position].index]
                .row_height()
                .max(1)
                + u16::from(heading(state, position, first).is_some())
        };
        let selected = self.selected_position().unwrap_or(0);
        self.scroll_top = self.scroll_top.min(self.matches.len() - 1).min(selected);
        while self.scroll_top < selected {
            let taken: u16 = (self.scroll_top..=selected)
                .map(|at| height(self, at, at == self.scroll_top))
                .sum();
            if taken <= area.height {
                break;
            }
            self.scroll_top += 1;
        }
        self.page = usize::from(area.height / height(self, self.scroll_top, true)).max(1);

        let gutter = if self.config.multi { GUTTER_WIDTH } else { 0 };
        let lead = MARKER_WIDTH + gutter;
        let content = Rect::new(
            area.x + lead,
            area.y,
            area.width.saturating_sub(lead + 1),
            area.height,
        );
        let mut y = area.y;
        for position in self.scroll_top..self.matches.len() {
            let found = &self.matches[position];
            let item = &self.items[found.index];
            let title = heading(self, position, position == self.scroll_top);
            let row_height = item.row_height().max(1);
            if y + row_height + u16::from(title.is_some()) > area.bottom() {
                break;
            }
            if let Some(title) = title {
                buf.set_stringn(
                    area.x + 1,
                    y,
                    &title,
                    usize::from(area.width.saturating_sub(2)),
                    theme.text_secondary.add_modifier(Modifier::BOLD),
                );
                y += 1;
            }
            let is_selected = position == selected;
            let cx = RowContext {
                width: content.width,
                selected: is_selected,
                filtered,
                theme,
                highlights: &found.highlights,
                hit: found.hit.as_ref(),
                terms: &self.terms,
                tick,
            };
            let rows = item.rows(&cx);
            let row_area = Rect::new(area.x, y, area.width, row_height);
            if is_selected {
                buf.set_style(row_area, theme.selected);
                buf.set_string(
                    area.x + 1,
                    y,
                    "▸",
                    theme.accent_violet.add_modifier(Modifier::BOLD),
                );
            }
            if self.config.multi {
                let (glyph, style) = if self.checked.contains(&item.id()) {
                    ("◼", theme.accent_mint)
                } else {
                    ("◻", theme.faint)
                };
                buf.set_string(area.x + MARKER_WIDTH, y, glyph, style);
            }
            for (offset, line) in rows.iter().take(usize::from(row_height)).enumerate() {
                let row_y = y + u16::try_from(offset).unwrap_or(0);
                buf.set_line(content.x, row_y, line, content.width);
            }
            self.drawn.push((position, y, row_height));
            y += row_height;
        }
    }

    fn render_preview(&mut self, title: Rect, pane: Rect, buf: &mut Buffer, theme: &Theme) {
        let Some(selected) = self.selected else {
            return;
        };
        self.sync_matches();
        let Some(preview) = self
            .preview
            .as_ref()
            .filter(|preview| preview.id == selected)
        else {
            return;
        };
        let title_area = Rect::new(
            title.x + PANE_PAD.min(title.width),
            title.y,
            title.width.saturating_sub(PANE_PAD),
            1,
        );
        let hits = self.match_lines.len();
        let badge = (hits > 0).then(|| {
            format!(
                "Hit {}/{hits}",
                self.preview_hit.unwrap_or(0).min(hits - 1) + 1
            )
        });
        let badge_width = badge
            .as_ref()
            .map_or(0, |text| u16::try_from(text.width()).unwrap_or(0) + 2);
        buf.set_line(
            title_area.x,
            title_area.y,
            &preview.title,
            title_area.width.saturating_sub(badge_width),
        );
        if let Some(badge) = &badge {
            let x = title_area
                .right()
                .saturating_sub(badge_width.saturating_sub(1));
            buf.set_stringn(
                x,
                title_area.y,
                badge,
                usize::from(badge_width),
                theme.faint,
            );
        }

        let body = Rect::new(
            pane.x + PANE_PAD.min(pane.width),
            pane.y,
            pane.width.saturating_sub(PANE_PAD + 1),
            pane.height,
        );
        let room = usize::from(body.height);
        let max_start = preview.lines.len().saturating_sub(room);
        let base = match (self.preview_hit, preview.anchor) {
            (Some(hit), _) if !self.match_lines.is_empty() => {
                self.match_lines[hit.min(self.match_lines.len() - 1)]
            }
            (_, PreviewAnchor::Top) => 0,
            (_, PreviewAnchor::Tail) => max_start,
            (_, PreviewAnchor::FirstMatch) => self.match_lines.first().copied().unwrap_or(0),
            (_, PreviewAnchor::Line(line)) => line,
        };
        let base = isize::try_from(base.min(max_start)).unwrap_or(0);
        let start = (base + self.preview_scroll).clamp(0, isize::try_from(max_start).unwrap_or(0));
        self.preview_scroll = start - base;
        let start = usize::try_from(start).unwrap_or(0);
        let emphasis = theme.accent_amber;
        for (offset, line) in preview.lines[start..].iter().take(room).enumerate() {
            let line = highlight_terms(line.clone(), &self.terms, emphasis);
            buf.set_line(
                body.x,
                body.y + u16::try_from(offset).unwrap_or(0),
                &line,
                body.width,
            );
        }
    }

    /// Find the preview lines that contain a query term, once per query and preview build.
    fn sync_matches(&mut self) {
        let Some(preview) = &self.preview else {
            self.match_lines.clear();
            self.match_for = None;
            return;
        };
        let key = (self.generation_key(), preview.built_at);
        if self.match_for == Some(key) {
            return;
        }
        self.match_lines = preview
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line_matches(line, &self.terms))
            .map(|(index, _)| index)
            .collect();
        self.match_for = Some(key);
    }

    /// Jump the preview to the next (`1`) or previous (`-1`) line containing a query term.
    pub(super) fn jump_hit(&mut self, delta: isize) {
        self.sync_matches();
        let count = isize::try_from(self.match_lines.len()).unwrap_or(0);
        if count == 0 {
            return;
        }
        let current = isize::try_from(self.preview_hit.unwrap_or(0)).unwrap_or(0);
        let next = if self.preview_hit.is_some() {
            current + delta
        } else {
            0
        };
        self.preview_hit = Some(usize::try_from(next.rem_euclid(count)).unwrap_or(0));
        self.preview_scroll = 0;
    }
}
