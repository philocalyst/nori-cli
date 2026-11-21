use super::*;

use crate::nori::session_header::ContextStatus;
use crate::nori::session_header::GitStatus;
use crate::nori::session_header::StatusFooterValues;

impl ChatComposer {
    /// Where the agent's activity may show in the prompt bar drawn in `area`: the prompt row
    /// (with how much of it the user's text takes) and the blank row along its top edge.
    pub(crate) fn activity_slots(
        &self,
        area: Rect,
    ) -> Option<crate::bottom_pane::activity::ActivitySlots> {
        if !self.input_enabled {
            return None;
        }
        let [composer_rect, textarea_rect, _] = self.layout_areas(area);
        // A composer squeezed below its height (a tall view over a short screen) lays rows out
        // past the area it was given; nothing is drawn there.
        if textarea_rect.is_empty() || textarea_rect.y >= area.bottom() {
            return None;
        }
        let typed = self
            .textarea
            .text()
            .lines()
            .next()
            .map_or(0, |line| {
                u16::try_from(unicode_width::UnicodeWidthStr::width(line)).unwrap_or(u16::MAX)
            })
            .saturating_add(LIVE_PREFIX_COLS);
        let prompt_row = Rect::new(composer_rect.x, textarea_rect.y, composer_rect.width, 1);
        let rail = (textarea_rect.y > composer_rect.y && composer_rect.y >= area.y)
            .then(|| Rect::new(composer_rect.x, composer_rect.y, composer_rect.width, 1));
        Some(crate::bottom_pane::activity::ActivitySlots {
            prompt_row: Some((prompt_row, typed)),
            rail,
        })
    }

    pub(super) fn layout_areas(&self, area: Rect) -> [Rect; 3] {
        let footer_props = self.footer_props();
        let footer_hint_height = self
            .custom_footer_height()
            .unwrap_or_else(|| footer_height(&footer_props));
        let footer_spacing = Self::footer_spacing(footer_hint_height);
        let footer_total_height = footer_hint_height + footer_spacing;
        let popup_constraint = match &self.active_popup {
            ActivePopup::Command(popup) => {
                Constraint::Max(popup.calculate_required_height(area.width))
            }
            ActivePopup::Skill(popup) => {
                Constraint::Max(popup.calculate_required_height(area.width))
            }
            ActivePopup::File(popup) => Constraint::Max(popup.calculate_required_height()),
            ActivePopup::HistorySearch(popup) => Constraint::Max(popup.calculate_required_height()),
            ActivePopup::Palette(popup) => {
                Constraint::Max(popup.calculate_required_height(area.width))
            }
            ActivePopup::None => Constraint::Max(footer_total_height),
        };
        let [composer_rect, popup_rect] =
            Layout::vertical([Constraint::Min(MIN_COMPOSER_HEIGHT), popup_constraint]).areas(area);
        let textarea_rect = composer_rect.inset(Insets::tlbr(1, LIVE_PREFIX_COLS, 1, 1));
        [composer_rect, textarea_rect, popup_rect]
    }

    pub(super) fn footer_spacing(footer_hint_height: u16) -> u16 {
        if footer_hint_height == 0 {
            0
        } else {
            FOOTER_SPACING_HEIGHT
        }
    }

    pub(super) fn footer_props(&self) -> FooterProps {
        let (
            git_branch,
            active_skillsets,
            nori_version,
            nori_version_source,
            git_lines_added,
            git_lines_removed,
            git_has_untracked,
        ) = if let Some(ref info) = self.system_info {
            (
                info.git_branch.clone(),
                info.active_skillsets.clone(),
                info.nori_version.clone(),
                info.nori_version_source,
                info.git_lines_added,
                info.git_lines_removed,
                info.git_has_untracked,
            )
        } else {
            (None, Vec::new(), None, None, None, None, false)
        };

        // The context the agent reports is the only measure of it: a turn's tokens are never
        // guessed from files the agent keeps.
        let (context_tokens, context_window_percent, context_window_tokens) =
            match &self.session_usage {
                Some(session_usage) if !self.child_context => (
                    Some(session_usage.used_tokens).filter(|&tokens| tokens >= 0),
                    (session_usage.total_tokens > 0).then(|| {
                        session_usage
                            .used_tokens
                            .saturating_mul(100)
                            .saturating_div(session_usage.total_tokens)
                            .clamp(0, 100)
                    }),
                    Some(session_usage.total_tokens).filter(|&tokens| tokens > 0),
                ),
                Some(_) | None if self.child_context => (None, None, None),
                Some(_) | None => (None, self.context_window_percent, None),
            };

        FooterProps {
            mode: FooterMode::ContextOnly,
            is_task_running: self.is_task_running,
            context_window_percent,
            context_tokens,
            context_window_tokens,
            git_branch,
            approval_mode_label: self.approval_mode_label.clone(),
            active_skillsets,
            nori_version,
            nori_version_source,
            git_lines_added,
            git_lines_removed,
            git_has_untracked,
            is_worktree: self
                .system_info
                .as_ref()
                .map(|s| s.is_worktree)
                .unwrap_or(false),
            input_tokens: None,
            output_tokens: None,
            cached_tokens: None,
            editor_mode: self
                .textarea
                .keymap()
                .is_modal()
                .then(|| self.textarea.mode()),
            prompt_summary: self.prompt_summary.clone(),
            session_title: self.session_title.clone(),
            worktree_name: self
                .system_info
                .as_ref()
                .and_then(|s| s.worktree_name.clone()),
            footer_segment_config: self.footer_segment_config.clone(),
            footer_layout_config: self.footer_layout_config.clone(),
            acp_mode_label: self.acp_mode_label.clone(),
            model_label: self.model_label.clone(),
            cloud_session: self.cloud_session.clone(),
            active_subagent_count: self.active_subagent_count,
            background: self.background,
            rewind_badge: match self.walk_badge() {
                Some(WalkBadge::Turn { turn, total }) => Some((turn as i64, total as i64)),
                Some(WalkBadge::History) | None => None,
            },
            bash_mode: self.is_shell_mode,
            status_note: self.status_note.clone(),
            key_help: if matches!(self.footer_mode(), FooterMode::ShortcutOverlay) {
                self.keymap
                    .help(self.textarea.hotkey_config(), self.use_shift_enter_hint)
            } else {
                Vec::new()
            },
            agent_cost: self
                .session_usage
                .as_ref()
                .and_then(|usage| usage.cost_display.clone()),
        }
    }

    /// Status-relevant footer values for the `/status` card, so the card stays
    /// a superset of the footer's information categories.
    pub(crate) fn status_footer_values(&self) -> StatusFooterValues {
        let props = self.footer_props();
        StatusFooterValues {
            git: GitStatus {
                branch: props.git_branch,
                is_worktree: props.is_worktree,
                worktree_name: props.worktree_name,
                lines_added: props.git_lines_added,
                lines_removed: props.git_lines_removed,
                has_untracked: props.git_has_untracked,
            },
            context: ContextStatus {
                tokens: props.context_tokens,
                window_tokens: props.context_window_tokens,
                percent_used: props.context_window_percent,
            },
            session_title: props.session_title,
            prompt_summary: props.prompt_summary,
            active_skillsets: props.active_skillsets,
            nori_version: props.nori_version,
            nori_version_source: props.nori_version_source,
            token_breakdown: None,
        }
    }

    /// Current context only: cumulative transcript billing totals must never
    /// be presented as the occupied portion of a context window.
    pub(crate) fn context_usage_snapshot(&self) -> (ContextStatus, &'static str) {
        if self.child_context {
            return (ContextStatus::default(), "Child usage unavailable");
        }
        if let Some(usage) = &self.session_usage {
            return (
                ContextStatus {
                    tokens: Some(usage.used_tokens).filter(|tokens| *tokens >= 0),
                    window_tokens: Some(usage.total_tokens).filter(|tokens| *tokens > 0),
                    percent_used: None,
                },
                "Agent-reported usage",
            );
        }
        (ContextStatus::default(), "No usage reported yet")
    }

    pub(super) fn footer_mode(&self) -> FooterMode {
        match self.footer_mode {
            FooterMode::ShortcutOverlay => FooterMode::ShortcutOverlay,
            FooterMode::CtrlCReminder => FooterMode::CtrlCReminder,
            FooterMode::ShortcutSummary if self.ctrl_c_quit_hint => FooterMode::CtrlCReminder,
            FooterMode::ShortcutSummary if !self.is_empty() => FooterMode::ContextOnly,
            other => other,
        }
    }

    pub(super) fn custom_footer_height(&self) -> Option<u16> {
        self.footer_hint_override
            .as_ref()
            .map(|items| if items.is_empty() { 0 } else { 1 })
    }
}
