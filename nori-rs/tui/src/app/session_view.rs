//! Focus and editing state for the main conversation and its child sessions.

use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;
use std::time::Instant;

use crate::app_event::AppEvent;
use crate::bottom_pane::ComposerDraft;
use crate::nori::agent_config_state::AgentConfigState;
use crate::subagent_switcher::Roster;
use crate::subagent_switcher::SubagentKey;
use crate::subagent_switcher::Target;
use crate::tui;
use ratatui::style::Stylize;

/// The shared chat view's active conversation and per-session editing state.
pub(crate) struct SessionView {
    focused: Target,
    order: Vec<Target>,
    last_cycle: Option<Instant>,
    drafts: HashMap<Target, ComposerDraft>,
    scroll_offsets: HashMap<Target, usize>,
    controlled_handles: HashMap<SubagentKey, nori_harness::runtime::HarnessHandle>,
    child_output: HashMap<SubagentKey, String>,
    main_scrolling: bool,
    drag_row: Option<u16>,
    strip_drag: Option<(u16, u16, Option<SubagentKey>, bool)>,
    expanded_tools: HashMap<Target, HashSet<String>>,
    windows: HashMap<Target, super::transcript_window::TranscriptWindow>,
    controlled_configs: HashMap<SubagentKey, AgentConfigState>,
    controls_loaded: HashSet<SubagentKey>,
    controls_revision: HashMap<SubagentKey, i64>,
    pub(super) child_history: Option<super::child_navigation::ChildHistory>,
}

impl Default for SessionView {
    fn default() -> Self {
        Self {
            focused: Target::Main,
            order: vec![Target::Main],
            last_cycle: None,
            drafts: HashMap::new(),
            scroll_offsets: HashMap::new(),
            controlled_handles: HashMap::new(),
            child_output: HashMap::new(),
            main_scrolling: false,
            drag_row: None,
            strip_drag: None,
            expanded_tools: HashMap::new(),
            windows: HashMap::new(),
            controlled_configs: HashMap::new(),
            controls_loaded: HashSet::new(),
            controls_revision: HashMap::new(),
            child_history: None,
        }
    }
}

impl SessionView {
    /// The tool calls shown expanded in `target`'s view, if any are.
    pub(super) fn invalidate_main_transcript_cell(&mut self, index: usize) {
        if let Some(window) = self.windows.get_mut(&Target::Main) {
            window.invalidate(index);
        }
    }

    fn reconcile(&mut self, roster: &Roster) {
        for (old, new) in &roster.replaced {
            let old_target = Target::Subagent(*old);
            let new_target = Target::Subagent(*new);
            if let Some(draft) = self.drafts.remove(&old_target) {
                self.drafts.entry(new_target).or_insert(draft);
            }
            if let Some(offset) = self.scroll_offsets.remove(&old_target) {
                self.scroll_offsets.entry(new_target).or_insert(offset);
            }
            if let Some(expanded) = self.expanded_tools.remove(&old_target) {
                self.expanded_tools
                    .entry(new_target)
                    .or_default()
                    .extend(expanded);
            }
            if let Some(output) = self.child_output.remove(old) {
                self.child_output.entry(*new).or_default().push_str(&output);
            }
            if let Some(handle) = self.controlled_handles.remove(old) {
                self.controlled_handles.entry(*new).or_insert(handle);
            }
            if let Some(config) = self.controlled_configs.remove(old) {
                self.controlled_configs.entry(*new).or_insert(config);
            }
            if self.controls_loaded.remove(old) {
                self.controls_loaded.insert(*new);
            }
        }
    }

    pub(crate) fn focused(&self) -> Target {
        self.focused
    }

    pub(crate) fn focus(&mut self, target: Target) -> Target {
        let previous = self.focused;
        self.order.retain(|item| *item != target);
        self.order.insert(0, target);
        self.focused = target;
        self.last_cycle = None;
        previous
    }

    /// A cycle has already selected its target and must retain its cycle timestamp.
    fn focus_if_changed(&mut self, target: Target) {
        if self.focused != target {
            self.focus(target);
        }
    }

    pub(crate) fn step(&mut self, roster: &Roster, reverse: bool, now: Instant) -> Target {
        self.reconcile(roster);
        self.order = self
            .order
            .iter()
            .map(|target| roster.resolve(*target))
            .collect();
        let mut seen = std::collections::HashSet::new();
        self.order
            .retain(|target| roster.entry(*target).is_some() && seen.insert(*target));
        self.focused = roster.resolve(self.focused);
        if roster.entry(self.focused).is_none() {
            self.focused = Target::Main;
        }
        if self
            .last_cycle
            .is_none_or(|last| now.duration_since(last) > Duration::from_secs(2))
        {
            self.order.retain(|target| *target != self.focused);
            self.order.insert(0, self.focused);
        }
        for target in
            std::iter::once(Target::Main).chain(roster.subagents.iter().map(|entry| entry.target))
        {
            if !self.order.contains(&target) {
                self.order.push(target);
            }
        }
        let index = self
            .order
            .iter()
            .position(|target| *target == self.focused)
            .unwrap_or(0);
        let next = if reverse {
            (index + self.order.len() - 1) % self.order.len()
        } else {
            (index + 1) % self.order.len()
        };
        self.focused = self.order[next];
        self.last_cycle = Some(now);
        self.focused
    }

    pub(crate) fn save_draft(&mut self, target: Target, draft: ComposerDraft) {
        self.drafts.insert(target, draft);
    }

    pub(crate) fn take_draft(&mut self, target: Target) -> Option<ComposerDraft> {
        self.drafts.remove(&target)
    }

    pub(crate) fn scroll_offset(&self) -> usize {
        self.scroll_offsets.get(&self.focused).copied().unwrap_or(0)
    }

    pub(crate) fn scroll(&mut self, lines: isize) {
        let offset = self.scroll_offsets.entry(self.focused).or_default();
        *offset = offset.saturating_add_signed(lines);
    }

    fn clamp_scroll(&mut self, max: usize) {
        if let Some(offset) = self.scroll_offsets.get_mut(&self.focused) {
            *offset = (*offset).min(max);
        }
    }

    pub(crate) fn main_scrolling(&self) -> bool {
        self.main_scrolling
    }

    pub(crate) fn start_drag(&mut self, row: u16) {
        self.drag_row = Some(row);
    }

    pub(crate) fn drag(&mut self, row: u16) -> isize {
        let Some(previous) = self.drag_row.replace(row) else {
            return 0;
        };
        isize::try_from(row).unwrap_or(isize::MAX) - isize::try_from(previous).unwrap_or(isize::MAX)
    }

    pub(crate) fn end_drag(&mut self) {
        self.drag_row = None;
    }

    pub(crate) fn start_strip_drag(&mut self, column: u16, row: u16, key: Option<SubagentKey>) {
        self.strip_drag = Some((column, row, key, false));
    }

    pub(crate) fn strip_drag(&mut self, column: u16, row: u16) -> isize {
        let Some((previous_column, previous_row, _, dragged)) = self.strip_drag.as_mut() else {
            return 0;
        };
        let column_delta = isize::try_from(column).unwrap_or(isize::MAX)
            - isize::try_from(*previous_column).unwrap_or(isize::MAX);
        let row_delta = isize::try_from(row).unwrap_or(isize::MAX)
            - isize::try_from(*previous_row).unwrap_or(isize::MAX);
        if column_delta.abs() < 3 && row_delta.abs() < 3 {
            return 0;
        }
        *previous_column = column;
        *previous_row = row;
        *dragged = true;
        if row_delta.abs() >= 3 {
            return 0;
        }
        -column_delta.signum()
    }

    pub(crate) fn end_strip_drag(&mut self) -> Option<SubagentKey> {
        self.strip_drag
            .take()
            .and_then(|(_, _, key, dragged)| if dragged { None } else { key })
    }

    pub(crate) fn cancel_strip_drag(&mut self) {
        self.strip_drag = None;
    }

    pub(crate) fn is_strip_dragging(&self) -> bool {
        self.strip_drag.is_some()
    }

    pub(crate) fn controls_loaded(&self, key: SubagentKey) -> bool {
        self.controls_loaded.contains(&key)
    }

    pub(super) fn advance_controls_revision(&mut self, key: SubagentKey) -> i64 {
        let revision = self.controls_revision.entry(key).or_default();
        *revision = revision.saturating_add(1);
        *revision
    }

    pub(super) fn controls_revision(&self, key: SubagentKey) -> i64 {
        self.controls_revision
            .get(&key)
            .copied()
            .unwrap_or_default()
    }

    pub(crate) fn set_controls(
        &mut self,
        key: SubagentKey,
        handle: Option<nori_harness::runtime::HarnessHandle>,
        config_options: &[nori_protocol::acp::v1::SessionConfigOption],
    ) {
        self.controls_loaded.insert(key);
        if let Some(handle) = handle {
            self.controlled_handles.insert(key, handle);
        }
        self.controlled_configs
            .insert(key, AgentConfigState::from_options(config_options));
    }

    pub(crate) fn controlled_handle(
        &self,
        key: SubagentKey,
    ) -> Option<nori_harness::runtime::HarnessHandle> {
        self.controlled_handles.get(&key).cloned()
    }

    pub(crate) fn controlled_config(&self, key: SubagentKey) -> Option<&AgentConfigState> {
        self.controlled_configs.get(&key)
    }

    pub(crate) fn append_child_output(&mut self, key: SubagentKey, text: &str) {
        self.child_output.entry(key).or_default().push_str(text);
    }

    pub(crate) fn append_child_notice(&mut self, key: SubagentKey, text: &str) {
        let output = self.child_output.entry(key).or_default();
        let notice = format!("\n{text}\n");
        if !output.ends_with(&notice) {
            output.push_str(&notice);
        }
    }
}

/// The id the stage gives an expandable card: its place in the conversation.
pub(crate) const CARD_PREFIX: &str = "cell:";

impl super::App {
    /// Expand or collapse the newest expandable card of the conversation, in place.
    pub(crate) fn open_latest_main_tool(&mut self, tui: &mut tui::Tui) {
        let Some(index) = self
            .transcript_cells
            .iter()
            .rposition(|cell| cell.expansion().is_some())
        else {
            return;
        };
        self.toggle_tool(tui, format!("{CARD_PREFIX}{index}"));
    }
    pub(crate) fn toggle_tool(&mut self, tui: &mut tui::Tui, call_id: String) {
        // A card on the stage is the cell itself: it holds its own expansion.
        if let Some(index) = call_id.strip_prefix(CARD_PREFIX)
            && let Some(cell) = index
                .parse::<usize>()
                .ok()
                .and_then(|index| self.transcript_cells.get(index))
        {
            cell.set_expanded(!cell.expansion().unwrap_or(false));
            tui.frame_requester().schedule_frame();
            return;
        }
        let target = self.session_view.focused();
        let expanded = self.session_view.expanded_tools.entry(target).or_default();
        if !expanded.remove(&call_id) {
            expanded.insert(call_id);
        }
        tui.frame_requester().schedule_frame();
    }

    pub(crate) fn scroll_main_session(&mut self, tui: &mut tui::Tui, lines: isize) {
        if self.session_view.focused() != Target::Main
            || self.overlay.is_some()
            || self.transcript_cells.is_empty()
        {
            return;
        }
        self.session_view.scroll(lines);
        self.session_view.main_scrolling = self.session_view.scroll_offset() > 0;
        tui.frame_requester().schedule_frame();
    }

    pub(crate) fn focus_session_view(&mut self, tui: &mut tui::Tui, target: Target) {
        let roster = self.chat_widget.subagent_roster(Instant::now());
        self.session_view.reconcile(&roster);
        let target = roster.resolve(target);
        let displayed = self
            .chat_widget
            .focused_subagent()
            .map(Target::Subagent)
            .unwrap_or(Target::Main);
        if target == displayed {
            return;
        }
        self.pending_child_focus = None;
        self.tool_gesture.cancel();
        self.close_focus(tui);
        if target == roster.resolve(displayed) {
            self.session_view.focus(target);
            if let Target::Subagent(key) = target {
                self.chat_widget.set_focused_subagent_view(
                    Some(key),
                    Vec::new(),
                    self.session_view.scroll_offset(),
                );
                self.request_subagent_controls(key);
            }
            tui.frame_requester().schedule_frame();
            return;
        }
        self.session_view.save_draft(
            roster.resolve(displayed),
            self.chat_widget.take_composer_draft(),
        );
        self.session_view.focus_if_changed(target);
        if let Some(draft) = self.session_view.take_draft(target) {
            self.chat_widget.restore_composer_draft(draft);
        }
        match target {
            Target::Main => {
                self.chat_widget
                    .set_focused_subagent_view(None, Vec::new(), 0);
            }
            Target::Subagent(key) => self.chat_widget.set_focused_subagent_view(
                Some(key),
                Vec::new(),
                self.session_view.scroll_offset(),
            ),
        }
        match target {
            Target::Main => self.chat_widget.set_focused_session_config(None, None),
            Target::Subagent(key) => {
                let display_name = roster
                    .entry(target)
                    .and_then(|entry| entry.agent.as_deref());
                self.chat_widget.set_focused_session_config(
                    Some(
                        self.session_view
                            .controlled_config(key)
                            .cloned()
                            .unwrap_or_default(),
                    ),
                    display_name,
                );
                self.request_subagent_controls(key);
            }
        }
        tui.frame_requester().schedule_frame();
    }

    fn request_subagent_controls(&mut self, key: SubagentKey) {
        if self.session_view.controls_loaded(key) {
            return;
        }
        self.refresh_subagent_controls(key);
    }

    pub(super) fn refresh_subagent_controls(&mut self, key: SubagentKey) {
        let Some(parent) = self.chat_widget.harness_handle() else {
            return;
        };
        let Some(session_id) = self.chat_widget.subagent_session(key) else {
            return;
        };
        let revision = self.session_view.advance_controls_revision(key);
        let app_event_tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let (handle, config_options, error) =
                match parent.controlled_subagent_handle(session_id.clone()).await {
                    Ok(Some(handle)) => {
                        let options = handle.get_session_config().await.unwrap_or_default();
                        (Some(handle), options, None)
                    }
                    Ok(None) => match parent.child_config_options(session_id).await {
                        Ok(options) => (None, options, None),
                        Err(error) => (None, Vec::new(), Some(error.to_string())),
                    },
                    Err(error) => (None, Vec::new(), Some(error.to_string())),
                };
            app_event_tx.send(AppEvent::SubagentControlsReady {
                key,
                revision,
                handle,
                config_options,
                error,
            });
        });
    }

    pub(crate) fn update_focused_session_view(
        &mut self,
        tui: &mut tui::Tui,
    ) -> color_eyre::eyre::Result<()> {
        let target = self.session_view.focused();
        if let Target::Subagent(key) = target
            && let Some(rendered) = self.focus_child_rows(tui.terminal.size()?.width)
        {
            self.session_view
                .clamp_scroll(rendered.lines.len().saturating_sub(1));
            self.chat_widget.set_focused_subagent_view(
                Some(key),
                rendered.lines,
                self.session_view.scroll_offset(),
            );
            return Ok(());
        }
        if target == Target::Main && !self.session_view.main_scrolling {
            // The conversation is the stage's: nothing is projected over it.
            self.chat_widget.clear_main_history_view();
            return Ok(());
        }
        if target == Target::Main && self.session_view.main_scrolling {
            let size = tui.terminal.size()?;
            let rows = size
                .height
                .saturating_sub(2)
                .saturating_sub(self.chat_widget.primary_composer_height(size.width))
                .saturating_sub(1)
                .max(1);
            let offset = self.session_view.scroll_offset();
            let hidden = self
                .hidden_transcript_len
                .and_then(|length| usize::try_from(length).ok())
                .unwrap_or(0)
                .min(self.transcript_cells.len());
            let cells = &self.transcript_cells[hidden..];
            let source = cells.last().map_or_else(
                || super::transcript_window::Version::Metadata(String::new()),
                |cell| super::transcript_window::Version::Main(cell.clone()),
            );
            let expanded = self.session_view.expanded_tools.entry(target).or_default();
            let window = self.session_view.windows.entry(target).or_default().render(
                cells.len(),
                size.width,
                usize::from(rows),
                offset,
                source,
                expanded,
                |index| {
                    cells
                        .get(index)
                        .map(|cell| super::transcript_window::Version::Main(cell.clone()))
                },
                |index| {
                    cells.get(index).map(|cell| {
                        let mut projection =
                            crate::chatwidget::tool_projection::ToolProjection::default();
                        projection.push_main(cell.as_ref(), size.width, expanded);
                        (projection, cell.is_stream_continuation())
                    })
                },
            );
            if let Some(maximum) = window.maximum_offset {
                self.session_view.clamp_scroll(maximum);
                if offset > maximum {
                    tui.frame_requester().schedule_frame();
                }
            }
            self.chat_widget
                .set_main_history_view(window.projection.lines, 0);
            self.chat_widget.set_transcript_follow_live(offset == 0);
            self.chat_widget
                .set_transcript_tools(window.projection.spans);
            return Ok(());
        }
        let Target::Subagent(_) = target else {
            self.chat_widget.clear_main_history_view();
            return Ok(());
        };
        let size = tui.terminal.size()?;
        let rows = size
            .height
            .saturating_sub(2)
            .saturating_sub(self.chat_widget.primary_composer_height(size.width))
            .saturating_sub(1)
            .max(1);
        let roster = self.chat_widget.subagent_roster(Instant::now());
        let target = roster.resolve(target);
        if target != self.session_view.focused() {
            self.focus_session_view(tui, target);
        }
        let Target::Subagent(key) = target else {
            return Ok(());
        };
        let metadata = roster.entry(target).map(|entry| {
            let mut metadata = vec![entry.title.clone()];
            if let Some(agent) = &entry.agent {
                metadata.push(agent.clone());
            }
            if let Some(model) = &entry.model {
                metadata.push(model.clone());
            }
            metadata.push(entry.status.word().to_owned());
            metadata.join(" · ")
        });
        let source_count = self.chat_widget.child_projection_len(key).unwrap_or(0);
        let output = self.session_view.child_output.get(&key);
        let count = 1 + source_count + usize::from(output.is_some());
        let source = super::transcript_window::Version::Metadata(format!(
            "{}:{}:{}",
            roster.entry(target).map_or(0, |entry| entry.revision),
            metadata.as_deref().unwrap_or_default(),
            output.map_or(0, String::len)
        ));
        let offset = self.session_view.scroll_offset();
        let expanded = self.session_view.expanded_tools.entry(target).or_default();
        let window = self.session_view.windows.entry(target).or_default().render(
            count,
            size.width,
            usize::from(rows),
            offset,
            source,
            expanded,
            |index| {
                if index == 0 {
                    Some(super::transcript_window::Version::Metadata(
                        metadata.clone().unwrap_or_default(),
                    ))
                } else if index <= source_count {
                    self.chat_widget
                        .child_projection_revision(key, index - 1)
                        .map(super::transcript_window::Version::Child)
                } else {
                    output.map(|value| {
                        super::transcript_window::Version::Output(
                            i64::try_from(value.len()).unwrap_or(i64::MAX),
                        )
                    })
                }
            },
            |index| {
                if index == 0 {
                    let mut projection =
                        crate::chatwidget::tool_projection::ToolProjection::default();
                    if let Some(metadata) = &metadata {
                        projection
                            .lines
                            .push(ratatui::text::Line::from(metadata.clone().bold()));
                        projection.lines.push(ratatui::text::Line::default());
                    }
                    Some((projection, true))
                } else if index <= source_count {
                    let projection = self.chat_widget.project_child_cell(
                        key,
                        index - 1,
                        size.width,
                        expanded,
                    )?;
                    let continuation = index == 1
                        || self
                            .chat_widget
                            .child_projection_continuation(key, index - 1)?;
                    Some((projection, continuation))
                } else {
                    let mut projection =
                        crate::chatwidget::tool_projection::ToolProjection::default();
                    for line in output?.lines() {
                        projection.lines.extend(
                            textwrap::wrap(line, usize::from(size.width).max(1))
                                .into_iter()
                                .map(|part| ratatui::text::Line::from(part.into_owned())),
                        );
                    }
                    Some((projection, true))
                }
            },
        );
        if let Some(maximum) = window.maximum_offset {
            self.session_view.clamp_scroll(maximum);
            if offset > maximum {
                tui.frame_requester().schedule_frame();
            }
        }
        self.chat_widget
            .set_focused_subagent_view(Some(key), window.projection.lines, 0);
        self.chat_widget
            .set_transcript_tools(window.projection.spans);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent_switcher::Entry;
    use crate::subagent_switcher::Status;
    use pretty_assertions::assert_eq;

    #[test]
    fn strip_swipe_scrolls_without_activating_a_pill() {
        let mut view = SessionView::default();
        view.start_strip_drag(15, 2, Some(SubagentKey(4)));
        assert_eq!(view.strip_drag(10, 2), 1);
        assert_eq!(view.end_strip_drag(), None);
        view.start_strip_drag(1, 2, Some(SubagentKey(3)));
        assert_eq!(view.end_strip_drag(), Some(SubagentKey(3)));
    }

    #[test]
    fn vertical_strip_drag_cancels_click_without_scrolling() {
        let mut view = SessionView::default();
        view.start_strip_drag(15, 2, Some(SubagentKey(4)));
        assert_eq!(view.strip_drag(15, 8), 0);
        assert_eq!(view.end_strip_drag(), None);
    }

    #[test]
    fn cancelling_strip_drag_discards_pending_click() {
        let mut view = SessionView::default();
        view.start_strip_drag(15, 2, Some(SubagentKey(4)));
        view.cancel_strip_drag();
        assert!(!view.is_strip_dragging());
        assert_eq!(view.end_strip_drag(), None);
    }

    fn roster(now: Instant) -> Roster {
        let entry = |target| Entry {
            target,
            title: "task".to_owned(),
            parent: None,
            agent: None,
            model: None,
            status: Status::Running,
            detail: None,
            transcript_text: String::new(),
            elapsed: Duration::ZERO,
            ended_ago: None,
            tool_count: 0,
            revision: 0,
            active_at: now,
        };
        Roster {
            main: entry(Target::Main),
            subagents: vec![
                entry(Target::Subagent(SubagentKey(1))),
                entry(Target::Subagent(SubagentKey(2))),
            ],
            replaced: vec![],
            animations: false,
            tick: 0,
        }
    }

    #[test]
    fn child_control_requests_are_versioned_per_session() {
        let mut view = SessionView::default();
        let first = SubagentKey(1);
        let second = SubagentKey(2);
        assert_eq!(view.advance_controls_revision(first), 1);
        assert_eq!(view.advance_controls_revision(second), 1);
        assert_eq!(view.advance_controls_revision(first), 2);
        assert_eq!(
            (
                view.controls_revision(first),
                view.controls_revision(second)
            ),
            (2, 1)
        );
    }

    #[test]
    fn native_child_controls_are_loaded_without_a_child_handle() {
        let mut view = SessionView::default();
        let key = SubagentKey(1);
        let options = vec![nori_protocol::acp::v1::SessionConfigOption::new(
            "fast-mode",
            "Fast mode",
            nori_protocol::acp::v1::SessionConfigKind::Boolean(
                nori_protocol::acp::v1::SessionConfigBoolean::new(true),
            ),
        )];
        view.set_controls(key, None, &options);
        assert!(view.controls_loaded(key));
        assert_eq!(view.controlled_config(key).unwrap().raw_options(), options);
        assert!(view.controlled_handle(key).is_none());
    }

    #[test]
    fn cycling_returns_to_the_most_recent_sessions_in_the_shared_view() {
        let now = Instant::now();
        let roster = roster(now);
        let mut view = SessionView::default();
        view.focus(Target::Subagent(SubagentKey(1)));
        view.focus(Target::Subagent(SubagentKey(2)));

        assert_eq!(
            view.step(&roster, false, now),
            Target::Subagent(SubagentKey(1))
        );
        assert_eq!(view.step(&roster, false, now), Target::Main);
        assert_eq!(
            view.step(&roster, true, now),
            Target::Subagent(SubagentKey(1))
        );
    }

    #[test]
    fn focus_handoff_preserves_a_three_session_cycle() {
        let now = Instant::now();
        let roster = roster(now);
        let mut view = SessionView::default();
        view.focus(Target::Subagent(SubagentKey(1)));
        view.focus(Target::Subagent(SubagentKey(2)));
        for expected in [
            Target::Subagent(SubagentKey(1)),
            Target::Main,
            Target::Subagent(SubagentKey(2)),
        ] {
            let target = view.step(&roster, false, now);
            view.focus_if_changed(target);
            assert_eq!(target, expected);
            assert_eq!(view.focused(), expected);
            assert_eq!(view.last_cycle, Some(now));
        }
        view.focus(Target::Subagent(SubagentKey(1)));
        assert_eq!(view.last_cycle, None);
    }

    #[test]
    fn each_session_keeps_its_own_scroll_offset() {
        let mut view = SessionView::default();
        view.scroll(8);
        view.focus(Target::Subagent(SubagentKey(1)));
        view.scroll(3);
        assert_eq!(view.scroll_offset(), 3);
        view.focus(Target::Main);
        assert_eq!(view.scroll_offset(), 8);
    }

    #[test]
    fn a_replaced_session_keeps_its_scroll_position() {
        let now = Instant::now();
        let mut roster = roster(now);
        roster.replaced.push((SubagentKey(1), SubagentKey(2)));
        let mut view = SessionView::default();
        view.focus(Target::Subagent(SubagentKey(1)));
        view.scroll(7);
        view.step(&roster, false, now);
        view.focus(Target::Subagent(SubagentKey(2)));
        assert_eq!(view.scroll_offset(), 7);
    }

    #[test]
    fn drag_tracks_direction_and_stops_on_release() {
        let mut view = SessionView::default();
        view.start_drag(8);
        assert_eq!(view.drag(11), 3);
        assert_eq!(view.drag(9), -2);
        view.end_drag();
        assert_eq!(view.drag(4), 0);
    }
}
