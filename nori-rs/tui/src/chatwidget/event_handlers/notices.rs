//! What Nori says in the transcript, kept apart from what the agent says.
//!
//! A notice never lands in the middle of the model's text: the answer being streamed is finished
//! first, so what follows the notice is a new message rather than a glued continuation. Notices
//! that arrive while a turn runs are one cell (see [`NoticeCell`]) instead of a blank row each.

use super::*;
use crate::history_cell::AgentTurnCell;
use crate::history_cell::AgentTurnKind;
use crate::history_cell::Notice;
use crate::history_cell::NoticeCell;
use crate::history_cell::NoticeKind;
use crate::history_cell::RouteEnd;
use crate::history_cell::RouteNotice;
use crate::history_cell::RouteNoticeCell;

impl ChatWidget {
    /// Say something as Nori.
    pub(in crate::chatwidget) fn push_notice(&mut self, kind: NoticeKind, text: String) {
        self.push_notice_with_hint(kind, text, None);
    }

    pub(in crate::chatwidget) fn push_notice_with_hint(
        &mut self,
        kind: NoticeKind,
        text: String,
        hint: Option<String>,
    ) {
        let notice = Notice { kind, text, hint };
        // While an answer streams a notice waits for it to be whole, joining the notices already
        // waiting; it is never put inside the model's words.
        if self.stream_controller.is_some() {
            if let Some(waiting) = self
                .deferred_cells
                .last_mut()
                .and_then(|cell| cell.as_any_mut().downcast_mut::<NoticeCell>())
            {
                waiting.push(notice);
            } else {
                self.deferred_cells.push(Box::new(NoticeCell::new(notice)));
            }
            return;
        }
        // A notice that follows another joins it; anything else (a tool, the answer) ends the run.
        if let Some(cell) = self
            .active_cell
            .as_mut()
            .and_then(|cell| cell.as_any_mut().downcast_mut::<NoticeCell>())
        {
            cell.push(notice);
        } else {
            self.flush_answer_stream_with_separator();
            self.flush_active_cell();
            self.active_cell
                .set(Some(Box::new(NoticeCell::new(notice))));
        }
        // With no turn running there is nothing to join: it goes to the transcript now.
        if !self.bottom_pane.is_task_running() {
            self.flush_active_cell();
        }
        self.request_redraw();
    }

    /// The session moved to another route after a failure: one calm row saying from where, to
    /// where and why, so the session never silently runs on something other than what it shows.
    pub(in crate::chatwidget) fn on_route_changed(
        &mut self,
        changed: &nori_protocol::RouteChanged,
    ) {
        let label = |route: &nori_protocol::RouteRef| match &route.model {
            Some(model) => format!(
                "{}/{}",
                route.agent,
                crate::history_cell::chrome::short_model(model)
            ),
            None => route.agent.clone(),
        };
        self.set_status_note(Some(crate::bottom_pane::StatusNote::Route {
            label: label(&changed.to),
            until: std::time::Instant::now() + std::time::Duration::from_secs(12),
        }));
        // A route on another agent changes what the header and status line name.
        if changed.to.agent != self.config.active_agent {
            self.config.active_agent = changed.to.agent.clone();
            self.config.agent = changed.to.agent.clone();
            self.show_agent_in_footer();
        }
        let end = |route: &nori_protocol::RouteRef, account: &Option<String>| RouteEnd {
            agent: route.agent.clone(),
            model: route.model.clone(),
            account: account.clone(),
        };
        self.on_route_notice(RouteNotice::Changed {
            from: end(&changed.from, &changed.from_account),
            to: end(&changed.to, &changed.to_account),
            reason: changed.cause.label().to_owned(),
        });
    }

    /// A failed request will be retried on the same route: one row saying when and why.
    pub(in crate::chatwidget) fn on_retry_scheduled(
        &mut self,
        retry: &nori_protocol::RetryScheduled,
    ) {
        self.set_status_note(Some(crate::bottom_pane::StatusNote::Retry {
            attempt: retry.attempt,
            max_attempts: retry.max_attempts,
            at: std::time::Instant::now()
                + std::time::Duration::from_millis(u64::try_from(retry.wait_ms).unwrap_or(0)),
        }));
        self.on_route_notice(RouteNotice::Retrying {
            attempt: i32::try_from(retry.attempt).unwrap_or(i32::MAX),
            max_attempts: i32::try_from(retry.max_attempts).unwrap_or(i32::MAX),
            wait: std::time::Duration::from_millis(u64::try_from(retry.wait_ms).unwrap_or(0)),
            reason: retry.class.label().to_owned(),
        });
    }

    /// The failed attempt's output stays above; one row says that it is obsolete. A turn that
    /// had streamed nothing has nothing to mark.
    pub(in crate::chatwidget) fn on_attempt_superseded(&mut self, by: nori_protocol::SupersededBy) {
        if self.stream_controller.is_none() {
            return;
        }
        self.on_route_notice(RouteNotice::Superseded {
            retried: by == nori_protocol::SupersededBy::Retry,
        });
    }

    /// A route change or a retry that is waiting, as its one row.
    pub(in crate::chatwidget) fn on_route_notice(&mut self, notice: RouteNotice) {
        self.add_side_cell(RouteNoticeCell::new(notice));
        self.request_redraw();
    }

    /// The agent started working without a request of ours. Said once, where the work begins; a
    /// turn that continues says nothing more.
    pub(in crate::chatwidget) fn on_agent_turn_started(
        &mut self,
        cause: nori_protocol::AgentTurnCause,
    ) {
        let already_running = matches!(
            self.acp_session_phase,
            Some(
                crate::presentation::session_runtime::SessionPhaseView::Prompt
                    | crate::presentation::session_runtime::SessionPhaseView::Cancelling
            )
        );
        if already_running {
            return;
        }
        if cause == nori_protocol::AgentTurnCause::Detached {
            self.set_status_note(Some(crate::bottom_pane::StatusNote::Detached));
        }
        self.add_side_cell(AgentTurnCell::new(match cause {
            nori_protocol::AgentTurnCause::Detached => AgentTurnKind::Detached,
            nori_protocol::AgentTurnCause::Observed => AgentTurnKind::Observed,
            nori_protocol::AgentTurnCause::Unprompted => AgentTurnKind::Unprompted,
        }));
    }

    /// Add a cell that is not part of the answer being streamed (a subagent that finished, a
    /// message between agents, a route change): now if nothing is streaming or the answer so far
    /// ends a paragraph, else after the message. A paragraph is never cut, and never flushed
    /// half a word, to make room for one.
    pub(in crate::chatwidget) fn add_side_cell(
        &mut self,
        cell: impl crate::history_cell::HistoryCell + 'static,
    ) {
        match &self.stream_controller {
            Some(stream) if !stream.at_paragraph_end() => self.deferred_cells.push(Box::new(cell)),
            Some(_) => {
                self.flush_answer_stream_with_separator();
                self.add_to_history(cell);
            }
            None => self.add_to_history(cell),
        }
    }

    /// Commit what waited for the message to be whole.
    pub(in crate::chatwidget) fn commit_deferred_cells(&mut self) {
        for cell in std::mem::take(&mut self.deferred_cells) {
            self.add_boxed_history(cell);
        }
    }
}
