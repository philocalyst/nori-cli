//! Everything the transcript is made of.
//!
//! A [`HistoryCell`] is one block of the conversation: it turns itself into rows for a width.
//! The cells are drawn from one design system ([`chrome`]) and live in modules by what they say:
//!
//! * [`prose`]: the user's prompt, reasoning, the model's answer;
//! * [`notice`]: what Nori says (information, warnings, errors), merged when adjacent;
//! * [`peer`]: peer messages, route changes and agent-owned turns;
//! * [`plan`]: the agent's plan;
//! * [`bento`] and [`shell_block`]: the grid and the command block that tool cells are built
//!   from.
//!
//! Tool calls are in `client_tool_cell`, subagents in `chatwidget::subagents`.

pub(crate) mod bento;
pub(crate) mod chrome;
mod monitor;
mod notice;
mod peer;
mod plan;
mod prose;
pub(crate) mod shell_block;

pub(crate) use monitor::MonitorEventsCell;
pub use notice::FinalMessageSeparator;
pub(crate) use notice::Notice;
pub(crate) use notice::NoticeCell;
pub(crate) use notice::NoticeKind;
pub(crate) use notice::PlainHistoryCell;
pub(crate) use notice::new_error_event;
pub(crate) use notice::new_info_event;
pub(crate) use notice::new_remote_control_event;
pub(crate) use notice::new_skillset_switched_event;
pub(crate) use peer::AgentTurnCell;
pub(crate) use peer::AgentTurnKind;
pub(crate) use peer::PeerDelivery;
pub(crate) use peer::PeerHandle;
pub(crate) use peer::PeerMessageCell;
pub(crate) use peer::PeerParty;
pub(crate) use peer::RouteEnd;
pub(crate) use peer::RouteNotice;
pub(crate) use peer::RouteNoticeCell;
pub(crate) use plan::new_plan_update;
pub(crate) use plan::render_plan_lines;
pub(crate) use prose::AgentMarkdownCell;
pub(crate) use prose::AgentMessageCell;
pub(crate) use prose::UserHistoryCell;

use crate::render::renderable::Renderable;
use crate::update_action::UpdateAction;
use crate::version::CODEX_CLI_VERSION;
use nori_config::NoriConfig as Config;
use prose::ReasoningSummaryCell;
use ratatui::prelude::*;
use ratatui::style::Stylize;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Wrap;
use std::any::Any;
use unicode_width::UnicodeWidthStr;

/// Represents an event to display in the conversation history. Returns its
/// `Vec<Line<'static>>` representation to make it easier to display in a
/// scrollable list.
pub(crate) trait HistoryCell: std::fmt::Debug + Send + Sync + Any {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>>;

    /// Logical lines for terminal scrollback, which the terminal may reflow.
    /// Structured cells keep their bounded layout unless they opt in.
    fn native_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }

    fn desired_height(&self, width: u16) -> u16 {
        Paragraph::new(Text::from(self.display_lines(width)))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .try_into()
            .unwrap_or(0)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.display_lines(width)
    }

    fn is_stream_continuation(&self) -> bool {
        false
    }

    /// Whether the transcript overlay shows every line of this cell instead of collapsing a long
    /// one. For cells that already are a collapsed form of something larger.
    fn full_transcript(&self) -> bool {
        false
    }

    /// Whether the cell's rows change with time alone (a spinner, a running clock). A layout
    /// cached for such a cell goes stale between updates, so it is laid out again each frame.
    fn animated(&self) -> bool {
        false
    }

    /// `Some(expanded)` when the cell can show more than its collapsed form (a diff beyond its
    /// first hunk rows, a command's whole output, a peer message's whole body); `None` when what
    /// it draws is all there is. Whoever routes the keys flips it with [`Self::set_expanded`].
    fn expansion(&self) -> Option<bool> {
        None
    }

    /// Expand or collapse the cell. Takes `&self`: cells in the transcript are shared, and the
    /// state is a flag the next draw reads.
    fn set_expanded(&self, _expanded: bool) {}
}

impl Renderable for Box<dyn HistoryCell> {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        let lines = self.display_lines(area.width);
        let y = if area.height == 0 {
            0
        } else {
            let overflow = lines.len().saturating_sub(usize::from(area.height));
            u16::try_from(overflow).unwrap_or(u16::MAX)
        };
        Paragraph::new(Text::from(lines))
            .scroll((y, 0))
            .render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        HistoryCell::desired_height(self.as_ref(), width)
    }
}

impl dyn HistoryCell {
    pub(crate) fn as_any(&self) -> &dyn Any {
        self
    }

    pub(crate) fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    pub(crate) fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[cfg_attr(debug_assertions, allow(dead_code))]
#[derive(Debug)]
pub(crate) struct UpdateAvailableHistoryCell {
    latest_version: String,
    update_action: Option<UpdateAction>,
}

#[cfg_attr(debug_assertions, allow(dead_code))]
impl UpdateAvailableHistoryCell {
    pub(crate) fn new(latest_version: String, update_action: Option<UpdateAction>) -> Self {
        Self {
            latest_version,
            update_action,
        }
    }
}

impl HistoryCell for UpdateAvailableHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        use ratatui_macros::line;
        use ratatui_macros::text;
        let update_instruction = if let Some(update_action) = self.update_action {
            line!["Run ", update_action.command_str().cyan(), " to update."]
        } else {
            line![
                "See ",
                "https://github.com/tilework-tech/nori-cli"
                    .cyan()
                    .underlined(),
                " for installation options."
            ]
        };

        let content = text![
            line![
                padded_emoji("✨").bold().cyan(),
                "Update available!".bold().cyan(),
                " ",
                format!("{CODEX_CLI_VERSION} -> {}", self.latest_version).bold(),
            ],
            update_instruction,
            "",
            "See full release notes:",
            "https://github.com/tilework-tech/nori-cli/releases/latest"
                .cyan()
                .underlined(),
        ];

        let inner_width = content
            .width()
            .min(usize::from(width.saturating_sub(4)))
            .max(1);
        with_border_with_inner_width(content.lines, inner_width)
    }
}

pub(crate) fn card_inner_width(width: u16, max_inner_width: usize) -> Option<usize> {
    if width < 4 {
        return None;
    }
    Some(std::cmp::min(
        width.saturating_sub(4) as usize,
        max_inner_width,
    ))
}

/// Render `lines` inside a border sized to the widest span in the content.
pub(crate) fn with_border(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    with_border_internal(lines, None)
}

/// Render `lines` inside a border whose inner width is at least `inner_width`.
///
/// This is useful when callers have already clamped their content to a
/// specific width and want the border math centralized here instead of
/// duplicating padding logic in the TUI widgets themselves.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn with_border_with_inner_width(
    lines: Vec<Line<'static>>,
    inner_width: usize,
) -> Vec<Line<'static>> {
    with_border_internal(lines, Some(inner_width))
}

fn with_border_internal(
    lines: Vec<Line<'static>>,
    forced_inner_width: Option<usize>,
) -> Vec<Line<'static>> {
    let max_line_width = lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum::<usize>()
        })
        .max()
        .unwrap_or(0);
    let content_width = forced_inner_width
        .unwrap_or(max_line_width)
        .max(max_line_width);

    let mut out = Vec::with_capacity(lines.len() + 2);
    let border_inner_width = content_width + 2;
    out.push(vec![format!("╭{}╮", "─".repeat(border_inner_width)).dim()].into());

    for line in lines.into_iter() {
        let used_width: usize = line
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum();
        let span_count = line.spans.len();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(span_count + 4);
        spans.push(Span::from("│ ").dim());
        spans.extend(line);
        if used_width < content_width {
            spans.push(Span::from(" ".repeat(content_width - used_width)).dim());
        }
        spans.push(Span::from(" │").dim());
        out.push(Line::from(spans));
    }

    out.push(vec![format!("╰{}╯", "─".repeat(border_inner_width)).dim()].into());

    out
}

/// Return the emoji followed by a hair space (U+200A).
/// Using only the hair space avoids excessive padding after the emoji while
/// still providing a small visual gap across terminals.
#[cfg_attr(debug_assertions, allow(dead_code))]
pub(crate) fn padded_emoji(emoji: &str) -> String {
    format!("{emoji}\u{200A}")
}

#[derive(Debug)]
pub struct SessionInfoCell(CompositeHistoryCell);

impl SessionInfoCell {
    pub(crate) fn new(composite: CompositeHistoryCell) -> Self {
        Self(composite)
    }
}

impl HistoryCell for SessionInfoCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.0.display_lines(width)
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.0.transcript_lines(width)
    }
}

pub(crate) fn new_session_info(
    config: &Config,
    agent: String,
    is_first_event: bool,
    status: crate::nori::session_header::StatusViewModel,
) -> SessionInfoCell {
    // Use the Nori-branded session header
    crate::nori::session_header::new_nori_session_info(config, agent, is_first_event, status)
}

pub(crate) fn new_user_prompt(message: String) -> UserHistoryCell {
    UserHistoryCell { message }
}

#[derive(Debug)]
pub(crate) struct CompositeHistoryCell {
    parts: Vec<Box<dyn HistoryCell>>,
}

impl CompositeHistoryCell {
    pub(crate) fn new(parts: Vec<Box<dyn HistoryCell>>) -> Self {
        Self { parts }
    }
}

impl HistoryCell for CompositeHistoryCell {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut out: Vec<Line<'static>> = Vec::new();
        let mut first = true;
        for part in &self.parts {
            let mut lines = part.display_lines(width);
            if !lines.is_empty() {
                if !first {
                    out.push(Line::from(""));
                }
                out.append(&mut lines);
                first = false;
            }
        }
        out
    }
}

pub(crate) fn new_reasoning_summary_block(full_reasoning_buffer: String) -> Box<dyn HistoryCell> {
    let trimmed = full_reasoning_buffer.trim();
    if let Some(after_open) = trimmed.strip_prefix("**")
        && let Some(close) = after_open.find("**")
    {
        let summary = after_open[(close + 2)..].trim();
        if !summary.is_empty() {
            return Box::new(ReasoningSummaryCell::new(summary.to_string(), false));
        }
    }

    Box::new(ReasoningSummaryCell::new(full_reasoning_buffer, true))
}
