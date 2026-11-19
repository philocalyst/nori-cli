use crate::bottom_pane::textarea::VimModeState;
use crate::key_hint;
use crate::system_info::NoriVersionSource;
use crate::ui_consts::FOOTER_INDENT_COLS;
use crate::ui_types::format_si_suffix;
use crossterm::event::KeyCode;
use nori_config::FooterFormatPart;
use nori_config::FooterLayoutConfig;
use nori_config::FooterLayoutItem;
use nori_config::FooterSegment;
use nori_config::FooterSegmentConfig;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Widget;

const MODE_INDICATOR_LABEL_MAX_CHARS: usize = 20;
/// Model names are short ("Opus 5.5"), but agents can advertise long ids.
const MODEL_LABEL_MAX_CHARS: usize = 34;
/// Minimum blank columns between the left and right footer groups before the
/// right group starts shedding segments.
const FOOTER_GROUP_MIN_GAP_COLS: u16 = 2;
/// Session titles are agent-controlled free text and routinely a whole
/// paragraph. The `/status` card keeps the longer form; the footer row has to
/// share one line with git, context, and approvals.
const SESSION_TITLE_MAX_CHARS: usize = 24;

#[derive(Clone, Debug)]
pub(crate) struct FooterProps {
    pub(crate) mode: FooterMode,
    pub(crate) is_task_running: bool,
    /// Context window percentage used (0-100).
    pub(crate) context_window_percent: Option<i64>,
    /// Tokens currently used in the context window.
    pub(crate) context_tokens: Option<i64>,
    /// Maximum tokens available in the context window.
    pub(crate) context_window_tokens: Option<i64>,
    pub(crate) git_branch: Option<String>,
    /// The approval mode label to display (e.g., "Read Only", "Agent", "Full Access").
    pub(crate) approval_mode_label: Option<String>,
    pub(crate) active_skillsets: Vec<String>,
    pub(crate) nori_version: Option<String>,
    /// The source of the version detection (affects display label).
    pub(crate) nori_version_source: Option<NoriVersionSource>,
    pub(crate) git_lines_added: Option<i32>,
    pub(crate) git_lines_removed: Option<i32>,
    pub(crate) git_has_untracked: bool,
    /// Whether the current directory is a git worktree (not the main repo).
    /// When true, the git branch indicator is shown in orange instead of yellow.
    pub(crate) is_worktree: bool,
    /// Input tokens from the external agent transcript, if available.
    pub(crate) input_tokens: Option<i64>,
    /// Output tokens from the external agent transcript, if available.
    pub(crate) output_tokens: Option<i64>,
    /// Cached tokens from the external agent transcript, if available.
    pub(crate) cached_tokens: Option<i64>,
    /// Vim mode state - only shown when vim mode is enabled.
    pub(crate) vim_mode_state: Option<VimModeState>,
    /// Short summary of the first user prompt for this session.
    pub(crate) prompt_summary: Option<String>,
    /// Agent-supplied session title from ACP session-info updates. Already
    /// sanitized and length-bounded by `crate::nori::session_info`.
    pub(crate) session_title: Option<String>,
    /// The worktree directory name (e.g., "good-ash-20260205-204831") when in a worktree.
    pub(crate) worktree_name: Option<String>,
    /// Configuration for which footer segments to show.
    pub(crate) footer_segment_config: FooterSegmentConfig,
    /// Configuration for where footer segments render.
    pub(crate) footer_layout_config: FooterLayoutConfig,
    /// ACP agent mode label to display when the agent exposes a mode option.
    pub(crate) acp_mode_label: Option<String>,
    /// The model the agent is using, when it offers a choice of model.
    pub(crate) model_label: Option<String>,
    /// Cloud session identifier (e.g. `nori-fast-kazunoko-aac8`) when attached
    /// through cloud mode. Renders the `CloudSession` segment.
    pub(crate) cloud_session: Option<String>,
    /// Number of currently active subagents shown as a compact statusline badge.
    pub(crate) active_subagent_count: i64,
    /// Monitors the agent armed that are watching now.
    pub(crate) monitor_count: i64,
    /// Active in-place rewind turn badge `(current, total)` when focusing past turns.
    pub(crate) rewind_badge: Option<(i64, i64)>,
    /// The composer holds a shell command (it starts with `!`).
    pub(crate) bash_mode: bool,
    /// What the session has cost so far, as the agent reported it.
    pub(crate) agent_cost: Option<String>,
    /// A route change, a retry countdown or the like, said in the status line for a while.
    pub(crate) status_note: Option<super::status_line::StatusNote>,
    /// The `?` list, from the keymap; filled only while it is open.
    pub(crate) key_help: Vec<nori_config::HelpRow>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FooterMode {
    /// A reminder that a second Ctrl+C quits, in place of the status line for a moment.
    CtrlCReminder,
    /// The status line.
    ShortcutSummary,
    /// The key list, opened with `?`.
    ShortcutOverlay,
    /// The status line while there is text in the composer.
    ContextOnly,
}

pub(crate) fn reset_mode_after_activity(current: FooterMode) -> FooterMode {
    match current {
        FooterMode::ShortcutOverlay | FooterMode::CtrlCReminder | FooterMode::ContextOnly => {
            FooterMode::ShortcutSummary
        }
        other => other,
    }
}

/// Whether the footer is the status line (and not a reminder or the key list).
fn is_status_line(mode: FooterMode) -> bool {
    matches!(mode, FooterMode::ShortcutSummary | FooterMode::ContextOnly)
}

pub(crate) fn footer_height(props: &FooterProps) -> u16 {
    if is_status_line(props.mode) {
        return 1;
    }
    footer_lines(props).len() as u16
}

pub(crate) fn render_footer(area: Rect, buf: &mut Buffer, props: &FooterProps) {
    if is_status_line(props.mode) {
        super::status_line::render(area, buf, props);
        return;
    }
    for (line_index, row) in footer_rows(props).into_iter().enumerate() {
        let Ok(line_offset) = u16::try_from(line_index) else {
            break;
        };
        if line_offset >= area.height {
            break;
        }

        let row_area = Rect {
            x: area.x,
            y: area.y + line_offset,
            width: area.width,
            height: 1,
        };
        render_footer_row(row_area, buf, row);
    }
}

fn footer_lines(props: &FooterProps) -> Vec<Line<'static>> {
    footer_rows(props)
        .into_iter()
        .map(FooterRow::joined)
        .collect()
}

#[derive(Clone, Debug, Default)]
struct FooterRow {
    left: Line<'static>,
    /// Right-aligned segments, kept separate rather than pre-joined so a narrow
    /// terminal can drop trailing ones instead of painting over the left group.
    right: Vec<Line<'static>>,
}

impl FooterRow {
    fn left(left: Line<'static>) -> Self {
        Self {
            left,
            right: Vec::new(),
        }
    }

    fn joined(self) -> Line<'static> {
        let right = join_footer_segments(&self.right);
        if right.spans.is_empty() {
            self.left
        } else if self.left.spans.is_empty() {
            right
        } else {
            let mut line = self.left;
            line.push_span(" · ".dim());
            line.extend(right.spans);
            line
        }
    }
}

fn render_footer_row(area: Rect, buf: &mut Buffer, row: FooterRow) {
    if area.is_empty() {
        return;
    }

    let footer_indent = u16::try_from(FOOTER_INDENT_COLS).unwrap_or(0);
    let left_area = Rect {
        x: area.x + footer_indent,
        y: area.y,
        width: area.width.saturating_sub(footer_indent),
        height: 1,
    };
    let left_width = line_cols(&row.left);
    row.left.render(left_area, buf);

    // The right group shares the row with the left one, so it only gets the
    // columns the indent and the left group leave behind. Shed trailing
    // segments until it fits; the group narrows and then disappears instead of
    // overwriting the left group on a small terminal.
    let mut reserved = footer_indent.saturating_add(left_width);
    if left_width > 0 {
        reserved = reserved.saturating_add(FOOTER_GROUP_MIN_GAP_COLS);
    }
    let Some(right) = fit_footer_segments(&row.right, area.width.saturating_sub(reserved)) else {
        return;
    };

    let right_width = line_cols(&right);
    let right_area = Rect {
        x: area.right().saturating_sub(right_width),
        y: area.y,
        width: right_width,
        height: 1,
    };
    right.render(right_area, buf);
}

fn line_cols(line: &Line<'static>) -> u16 {
    u16::try_from(line.width()).unwrap_or(u16::MAX)
}

/// Join the longest leading run of `segments` that fits in `available` columns.
/// Returns `None` when nothing fits, which hides the group for that frame.
fn fit_footer_segments(segments: &[Line<'static>], available: u16) -> Option<Line<'static>> {
    (1..=segments.len())
        .rev()
        .map(|end| join_footer_segments(&segments[..end]))
        .find(|candidate| !candidate.spans.is_empty() && line_cols(candidate) <= available)
}

fn footer_rows(props: &FooterProps) -> Vec<FooterRow> {
    match props.mode {
        FooterMode::CtrlCReminder => {
            vec![FooterRow::left(ctrl_c_reminder_line(CtrlCReminderState {
                is_task_running: props.is_task_running,
            }))]
        }
        FooterMode::ShortcutOverlay => key_list_lines(&props.key_help)
            .into_iter()
            .map(FooterRow::left)
            .collect(),
        // Drawn by the status line, which is not made of rows of segments.
        FooterMode::ShortcutSummary | FooterMode::ContextOnly => Vec::new(),
    }
}

#[derive(Clone, Copy, Debug)]
struct CtrlCReminderState {
    is_task_running: bool,
}

fn ctrl_c_reminder_line(state: CtrlCReminderState) -> Line<'static> {
    let action = if state.is_task_running {
        "interrupt"
    } else {
        "quit"
    };
    Line::from(vec![
        key_hint::ctrl(KeyCode::Char('c')).into(),
        format!(" again to {action}").into(),
    ])
    .dim()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TextareaCornerSegments {
    pub(crate) top_left: Line<'static>,
    pub(crate) top_right: Line<'static>,
    pub(crate) bottom_left: Line<'static>,
    pub(crate) bottom_right: Line<'static>,
}

pub(crate) fn textarea_corner_segments(props: &FooterProps) -> TextareaCornerSegments {
    TextareaCornerSegments {
        top_left: join_footer_segments(&segments_for(
            props,
            &props.footer_layout_config.textarea_top_left,
        )),
        top_right: join_footer_segments(&segments_for(
            props,
            &props.footer_layout_config.textarea_top_right,
        )),
        bottom_left: join_footer_segments(&segments_for(
            props,
            &props.footer_layout_config.textarea_bottom_left,
        )),
        bottom_right: join_footer_segments(&segments_for(
            props,
            &props.footer_layout_config.textarea_bottom_right,
        )),
    }
}

fn segments_for(props: &FooterProps, segment_order: &[FooterLayoutItem]) -> Vec<Line<'static>> {
    segment_order
        .iter()
        .filter_map(|item| footer_layout_item(props, item))
        .collect()
}

fn footer_layout_item(props: &FooterProps, item: &FooterLayoutItem) -> Option<Line<'static>> {
    match item {
        FooterLayoutItem::Builtin(segment) if props.footer_segment_config.is_enabled(*segment) => {
            builtin_footer_segment(props, *segment)
        }
        FooterLayoutItem::Builtin(_) => None,
        FooterLayoutItem::Custom(format) => {
            let mut line = Line::from("");
            for part in format.parts() {
                match part {
                    FooterFormatPart::Text(text) => line.push_span(text.clone()),
                    FooterFormatPart::Segment(segment) => {
                        let segment = builtin_footer_segment(props, *segment)?;
                        line.extend(segment.spans);
                    }
                }
            }
            (!line.spans.is_empty()).then_some(line)
        }
    }
}

fn builtin_footer_segment(props: &FooterProps, segment: FooterSegment) -> Option<Line<'static>> {
    match segment {
        FooterSegment::PromptSummary => props
            .prompt_summary
            .as_ref()
            .map(|summary| Line::from(vec!["Task: ".dim(), Span::from(summary.clone()).dim()])),
        FooterSegment::SessionTitle => props.session_title.as_ref().map(|title| {
            let title = truncate_label(title, SESSION_TITLE_MAX_CHARS);
            Line::from(vec!["Title: ".dim(), Span::from(title).dim()])
        }),
        FooterSegment::VimMode => props.vim_mode_state.map(|vim_state| {
            let (label, style_fn): (&str, fn(Span<'static>) -> Span<'static>) = match vim_state {
                VimModeState::Normal => ("NORMAL", |s| s.light_blue().bold()),
                VimModeState::Insert => ("INSERT", |s| s.green()),
            };
            Line::from(vec![style_fn(Span::from(label))])
        }),
        FooterSegment::GitBranch => props.git_branch.as_ref().map(|branch| {
            if props.is_worktree {
                #[allow(clippy::disallowed_methods)]
                Line::from(vec![
                    Span::from("git ").light_red(),
                    Span::from(branch.clone()).light_red(),
                ])
            } else {
                #[allow(clippy::disallowed_methods)]
                Line::from(vec![
                    Span::from("git ").yellow(),
                    Span::from(branch.clone()).yellow(),
                ])
            }
        }),
        FooterSegment::WorktreeName => props.worktree_name.as_ref().map(|name| {
            #[allow(clippy::disallowed_methods)]
            Line::from(vec![
                Span::from("Worktree: ").light_red(),
                Span::from(name.clone()).light_red(),
            ])
        }),
        FooterSegment::GitStats => git_stats_segment(props),
        FooterSegment::Context => context_segment(props),
        FooterSegment::ContextUsedPercent => context_used_percent_segment(props),
        FooterSegment::ContextRemainingPercent => context_remaining_percent_segment(props),
        FooterSegment::ContextUsedTokens => context_used_tokens_segment(props),
        FooterSegment::ContextRemainingTokens => context_remaining_tokens_segment(props),
        FooterSegment::ContextWindowTokens => context_window_tokens_segment(props),
        FooterSegment::ApprovalMode => props.approval_mode_label.as_ref().map(|label| {
            Line::from(vec![
                Span::from("Approvals: ").magenta(),
                Span::from(label.clone()).magenta(),
            ])
        }),
        FooterSegment::Skillset => skillset_segment(props),
        FooterSegment::NoriVersion => props.nori_version.as_ref().map(|version| {
            let label = props
                .nori_version_source
                .map(NoriVersionSource::label)
                .unwrap_or("Skillsets");
            Line::from(vec![
                Span::from(format!("{label} v")).green(),
                Span::from(version.clone()).green(),
            ])
        }),
        FooterSegment::TokenUsage => token_usage_segment(props),
        FooterSegment::Model => props.model_label.as_ref().map(|label| {
            Line::from(Span::from(truncate_label(label, MODEL_LABEL_MAX_CHARS)).dim())
        }),
        FooterSegment::ModeIndicator => props.acp_mode_label.as_ref().map(|raw_label| {
            let label = truncate_label(raw_label, MODE_INDICATOR_LABEL_MAX_CHARS);
            let text = format!("[ {label} ]");
            let lower = raw_label.to_lowercase();
            let span = if lower.contains("plan") {
                Span::from(text).green()
            } else if lower.contains("bypass") {
                Span::from(text).red()
            } else if lower.contains("accept")
                || lower.contains("implement")
                || lower.contains("build")
            {
                Span::from(text).cyan()
            } else {
                Span::from(text).dim()
            };
            Line::from(span)
        }),
        FooterSegment::CloudSession => props
            .cloud_session
            .as_ref()
            .map(|id| Line::from(vec![Span::from("☁ ").cyan(), Span::from(id.clone()).cyan()])),
    }
}

fn truncate_label(label: &str, max_chars: usize) -> String {
    if label.chars().count() <= max_chars {
        return label.to_string();
    }

    label
        .chars()
        .take(max_chars.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

fn git_stats_segment(props: &FooterProps) -> Option<Line<'static>> {
    let mut spans = Vec::new();
    if let (Some(added), Some(removed)) = (props.git_lines_added, props.git_lines_removed)
        && (added > 0 || removed > 0)
    {
        spans.extend([
            Span::from(format!("+{added}")).green(),
            Span::from(" ").dim(),
            Span::from(format!("-{removed}")).red(),
        ]);
    }
    if props.git_has_untracked {
        if !spans.is_empty() {
            spans.push(Span::from(" ").dim());
        }
        spans.push(Span::from("!").red().bold());
    }
    (!spans.is_empty()).then(|| Line::from(spans))
}

fn context_segment(props: &FooterProps) -> Option<Line<'static>> {
    let formatted_used_tokens = props
        .context_tokens
        .filter(|&tokens| tokens > 0)
        .map(format_context_tokens);
    let formatted_window_tokens = props
        .context_window_tokens
        .filter(|&tokens| tokens > 0)
        .map(format_context_tokens);
    let context_text = match (
        props.context_window_percent.map(clamp_percent),
        formatted_window_tokens,
        formatted_used_tokens,
    ) {
        (Some(percent), Some(window), _) => Some(format!("{percent}% / {window}")),
        (Some(percent), None, _) => Some(format!("{percent}%")),
        (None, _, Some(used)) => Some(used),
        (None, _, None) => None,
    };
    context_text.map(Line::from)
}

fn context_used_percent_segment(props: &FooterProps) -> Option<Line<'static>> {
    props
        .context_window_percent
        .map(clamp_percent)
        .map(|percent| Line::from(format!("{percent}%")))
}

fn context_remaining_percent_segment(props: &FooterProps) -> Option<Line<'static>> {
    props
        .context_window_percent
        .map(clamp_percent)
        .map(|percent| Line::from(format!("{}%", 100 - percent)))
}

fn context_used_tokens_segment(props: &FooterProps) -> Option<Line<'static>> {
    props
        .context_tokens
        .filter(|&tokens| tokens >= 0)
        .map(format_context_tokens)
        .map(Line::from)
}

fn context_remaining_tokens_segment(props: &FooterProps) -> Option<Line<'static>> {
    props
        .context_window_tokens
        .zip(props.context_tokens)
        .filter(|(window, used)| *window > 0 && *used >= 0)
        .map(|(window, used)| window.saturating_sub(used).max(0))
        .map(format_context_tokens)
        .map(Line::from)
}

fn context_window_tokens_segment(props: &FooterProps) -> Option<Line<'static>> {
    props
        .context_window_tokens
        .filter(|&tokens| tokens > 0)
        .map(format_context_tokens)
        .map(Line::from)
}

fn clamp_percent(percent: i64) -> i64 {
    percent.clamp(0, 100)
}

fn format_context_tokens(tokens: i64) -> String {
    let mut formatted = format_si_suffix(tokens);
    if formatted.ends_with('K') {
        formatted.pop();
        formatted.push('k');
    }
    formatted
}

fn skillset_segment(props: &FooterProps) -> Option<Line<'static>> {
    if props.active_skillsets.is_empty() {
        return None;
    }
    let label = if props.active_skillsets.len() == 1 {
        "Skillset: "
    } else {
        "Skillsets: "
    };
    Some(Line::from(vec![
        Span::from(label).cyan(),
        Span::from(props.active_skillsets.join(", ")).cyan(),
    ]))
}

fn token_usage_segment(props: &FooterProps) -> Option<Line<'static>> {
    let input = props.input_tokens.unwrap_or(0);
    let output = props.output_tokens.unwrap_or(0);
    let cached = props.cached_tokens.unwrap_or(0);
    let total = input.saturating_add(output).saturating_add(cached);
    if total == 0 {
        return None;
    }

    let total_fmt = format_si_suffix(total);
    let mut spans = vec![
        "Tokens: ".dim(),
        Span::from(format!("{total_fmt} total")).dim(),
    ];
    if cached > 0 {
        let cached_fmt = format_si_suffix(cached);
        spans.push(Span::from(format!(" ({cached_fmt} cached)")).dim());
    }

    Some(Line::from(spans))
}

fn join_footer_segments(segments: &[Line<'static>]) -> Line<'static> {
    let mut line = Line::from("");
    for (idx, segment) in segments.iter().enumerate() {
        if idx > 0 {
            line.push_span(" · ".dim());
        }
        line.extend(segment.spans.clone());
    }
    line
}

/// The `?` list: one row per key entry of the keymap, as bound now.
fn key_list_lines(rows: &[nori_config::HelpRow]) -> Vec<Line<'static>> {
    let theme = crate::style::component_theme();
    let width = rows.iter().map(|row| row.keys.len()).max().unwrap_or(0);
    rows.iter()
        .map(|row| {
            Line::from(vec![
                Span::styled(format!("{:<width$}  ", row.keys), theme.accent_cyan),
                Span::styled(row.what.to_owned(), theme.text_secondary),
            ])
        })
        .collect()
}
