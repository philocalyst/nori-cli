//! The usage view's frame and the screen inside it, laid out top to bottom.

use chrono::DateTime;
use chrono::Utc;
use nori_harness::usage::UsagePeriod;
use nori_harness::usage::UsageSummary;
use nori_tui_components::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;
use unicode_width::UnicodeWidthStr;

use super::accounts::accounts;
use super::breakdown::breakdown;
use super::chart::chart;
use super::detail::detail;
use super::header::Header;
use super::header::Hit;
use super::header::render_header;
use super::limits::limits;
use super::overview::overview;
use super::prices::prices;
use super::shares::shares;
use super::totals::totals;
use super::view::Metric;
use super::view::ModelKey;
use super::view::Screen;
use super::view::View;

/// Everything a frame needs, borrowed from the overlay.
pub(super) struct Frame<'a> {
    pub theme: &'a Theme,
    pub period: UsagePeriod,
    pub metric: Metric,
    /// The provider the summary is restricted to.
    pub provider: Option<&'a str>,
    pub screen: &'a Screen,
    pub selected: Option<&'a ModelKey>,
    pub summary: Option<&'a UsageSummary>,
    pub now: DateTime<Utc>,
    pub scroll: i32,
}

/// What a frame learned that the overlay needs for the next event.
pub(super) struct Drawn {
    pub hits: Vec<(Rect, Hit)>,
    /// Rows of content and rows shown, for clamping the scroll.
    pub content_rows: i32,
    pub viewport_rows: i32,
    /// The content row of every model in the breakdown, in table order.
    pub model_rows: Vec<(i32, ModelKey)>,
}

/// The lines of the screen, and where the model rows are in them.
pub(super) struct Content {
    pub lines: Vec<Line<'static>>,
    pub model_rows: Vec<(i32, ModelKey)>,
}

pub(super) fn content(view: &View, screen: &Screen) -> Content {
    let summary = view.summary;
    let mut content = Content {
        lines: Vec::new(),
        model_rows: Vec::new(),
    };
    match screen {
        Screen::Model(key) => content.lines = detail(view, key),
        Screen::Prices => content.lines = prices(view),
        Screen::Overview => overview_screen(view, &mut content),
    }
    if summary.skipped_lines > 0 {
        content.lines.push(Line::default());
        content.lines.push(Line::from(Span::styled(
            format!(
                "{} unreadable line{} in the usage log {} skipped.",
                summary.skipped_lines,
                if summary.skipped_lines == 1 { "" } else { "s" },
                if summary.skipped_lines == 1 {
                    "was"
                } else {
                    "were"
                },
            ),
            view.theme.faint,
        )));
    }
    content
}

fn overview_screen(view: &View, content: &mut Content) {
    let summary = view.summary;
    if summary.is_empty() && summary.limits.is_empty() {
        content.lines = vec![
            Line::default(),
            Line::from(Span::styled(
                "No usage recorded yet.",
                view.theme.text_secondary,
            )),
            Line::from(Span::styled(
                "Agents that report usage appear here after their next turn.",
                view.theme.faint,
            )),
        ];
        return;
    }
    let mut sections = vec![
        overview(view),
        totals(view),
        chart(view),
        limits(view),
        accounts(view),
        shares(view),
    ];
    let table = (!summary.models.is_empty()).then(|| breakdown(view));
    for section in sections.drain(..).filter(|section| !section.is_empty()) {
        if !content.lines.is_empty() {
            content.lines.push(Line::default());
        }
        content.lines.extend(section);
    }
    if let Some(table) = table {
        if !content.lines.is_empty() {
            content.lines.push(Line::default());
        }
        let offset = i32::try_from(content.lines.len()).unwrap_or(0);
        content.model_rows.extend(
            table
                .rows
                .into_iter()
                .map(|(row, key)| (offset + i32::try_from(row).unwrap_or(0), key)),
        );
        content.lines.extend(table.lines);
    }
}

pub(super) fn render(frame: &Frame, area: Rect, buf: &mut Buffer) -> Drawn {
    Clear.render(area, buf);
    let theme = frame.theme;
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(theme.faint)
        .title(Line::from(vec![
            Span::styled(" ◇ ", theme.accent_cyan),
            Span::styled("Usage ", theme.text_strong),
        ]));
    let inner = block.inner(area);
    block.render(area, buf);
    let mut drawn = Drawn {
        hits: Vec::new(),
        content_rows: 0,
        viewport_rows: 0,
        model_rows: Vec::new(),
    };
    if inner.width < 20 || inner.height < 4 {
        return drawn;
    }

    let pad = 2;
    render_header(
        &Header {
            theme,
            period: frame.period,
            metric: frame.metric,
            provider: frame.provider,
            prices_open: *frame.screen == Screen::Prices,
        },
        Rect::new(
            inner.x + pad,
            inner.y,
            inner.width.saturating_sub(pad * 2),
            1,
        ),
        buf,
        &mut drawn.hits,
    );

    // Rule between header and content, joined to the frame.
    let rule_y = inner.y + 1;
    let rule = format!(
        "├{}┤",
        "─".repeat(usize::from(area.width.saturating_sub(2)))
    );
    Paragraph::new(Line::from(Span::styled(rule, theme.faint)))
        .render(Rect::new(area.x, rule_y, area.width, 1), buf);

    let body = Rect::new(
        inner.x + pad,
        rule_y + 1,
        inner.width.saturating_sub(pad * 2),
        inner.height.saturating_sub(2),
    );
    let lines = match frame.summary {
        Some(summary) => {
            let view = View {
                theme,
                selected: frame.selected,
                width: i32::from(body.width),
                metric: frame.metric,
                now: frame.now,
                summary,
            };
            let content = content(&view, frame.screen);
            drawn.model_rows = content.model_rows;
            content.lines
        }
        None => vec![
            Line::default(),
            Line::from(Span::styled("Reading usage…", theme.faint)),
        ],
    };
    drawn.content_rows = i32::try_from(lines.len()).unwrap_or(i32::MAX);
    drawn.viewport_rows = i32::from(body.height);
    let max_scroll = (drawn.content_rows - drawn.viewport_rows).max(0);
    let scroll = frame.scroll.clamp(0, max_scroll);
    Paragraph::new(lines)
        .scroll((u16::try_from(scroll).unwrap_or(0), 0))
        .render(body, buf);

    for (row, key) in &drawn.model_rows {
        let y = row - scroll;
        if (0..drawn.viewport_rows).contains(&y) {
            drawn.hits.push((
                Rect::new(
                    body.x,
                    body.y + u16::try_from(y).unwrap_or(0),
                    body.width,
                    1,
                ),
                Hit::Model(key.clone()),
            ));
        }
    }

    if max_scroll > 0 {
        let label = format!(" {}% ", (scroll * 100) / max_scroll);
        let width = u16::try_from(label.width()).unwrap_or(0);
        if area.width > width + 3 {
            Paragraph::new(Line::from(Span::styled(label, theme.faint))).render(
                Rect::new(area.right() - width - 2, area.bottom() - 1, width, 1),
                buf,
            );
        }
    }
    drawn
}
