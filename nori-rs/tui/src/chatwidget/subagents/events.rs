//! Wiring of subagent events into the chat widget.

use std::time::Duration;
use std::time::Instant;

use nori_protocol::ClaudeToolMeta;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

use super::ChildTarget;
use super::SubagentSummaryCell;
use crate::chatwidget::ChatWidget;
use crate::history_cell::PeerMessageCell;

/// How often the live block is redrawn while a child runs, for its spinner and clock.
const ANIMATION_FRAME: Duration = Duration::from_millis(80);
/// Without animations only the elapsed seconds change.
const CLOCK_FRAME: Duration = Duration::from_secs(1);

impl ChatWidget {
    /// A subagent announcement, state change, or message between sessions.
    pub(in crate::chatwidget) fn handle_subagent_wire(&mut self, wire: SubagentWire) {
        let now = Instant::now();
        match wire {
            SubagentWire::Update(update) => self.subagents.on_update(update, now),
            SubagentWire::Message(message) => self.subagents.on_message(message, now),
        }
        self.after_subagent_change();
    }

    /// An update delivered on a child session.
    pub(in crate::chatwidget) fn handle_child_notification(
        &mut self,
        notification: &acp::SessionNotification,
    ) {
        self.subagents.on_child_update(
            ChildTarget::Session(&notification.session_id),
            &notification.update,
            Instant::now(),
        );
        self.after_subagent_change();
    }

    /// Take a root-session update that belongs to a subagent: the call that launches one, and
    /// anything tagged with the id of such a call. Returns whether the update was taken.
    ///
    /// This is what shows subagents for an agent that does not announce them.
    pub(in crate::chatwidget) fn route_root_update_to_subagents(
        &mut self,
        notification: &acp::SessionNotification,
    ) -> bool {
        let update = &notification.update;
        let (meta, call_id) = match update {
            acp::SessionUpdate::ToolCall(call) => (call.meta.as_ref(), Some(&call.tool_call_id)),
            acp::SessionUpdate::ToolCallUpdate(call) => {
                (call.meta.as_ref(), Some(&call.tool_call_id))
            }
            acp::SessionUpdate::AgentMessageChunk(chunk)
            | acp::SessionUpdate::AgentThoughtChunk(chunk) => (chunk.meta.as_ref(), None),
            _ => return false,
        };
        let meta = ClaudeToolMeta::read(meta);
        let now = Instant::now();
        if let Some(launcher) = meta.parent_tool_use_id {
            self.subagents
                .on_child_update(ChildTarget::Launcher(launcher), update, now);
            self.after_subagent_change();
            return true;
        }
        let Some(call_id) = call_id else {
            return false;
        };
        let launches = matches!(update, acp::SessionUpdate::ToolCall(_)) && meta.launches_subagent;
        if !launches && !self.subagents.is_launcher_call(&call_id.0) {
            return false;
        }
        self.subagents
            .on_launcher_update(update, meta.async_launch, now);
        self.after_subagent_change();
        true
    }

    /// The root turn ended; children that only a launcher call stood for end with it.
    pub(in crate::chatwidget) fn subagents_turn_ended(&mut self, cancelled: bool) {
        self.subagents.close_peer_messages();
        self.subagents.on_root_turn_end(cancelled, Instant::now());
        self.after_subagent_change();
    }

    /// The label for a request that came from a child session, to show on its approval.
    pub(in crate::chatwidget) fn subagent_label_for(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<String> {
        if self.subagents.is_root(session_id) {
            return None;
        }
        self.subagents.label_for_session(session_id)
    }

    /// Commit children that finished, refresh the status line, and keep the clock moving.
    fn after_subagent_change(&mut self) {
        self.commit_finished_subagents();
        let running = self.subagents.running_count();
        self.bottom_pane
            .set_active_subagent_count(i64::try_from(running).unwrap_or(0));
        if running != self.subagents_running_shown {
            self.subagents_running_shown = running;
            let header = self.current_status_header.clone();
            self.set_status_header(header);
        }
        self.request_redraw();
    }

    /// Peer messages that became whole and children whose end can be believed, as cells. They
    /// are side cells: while an answer streams they wait for it to be whole.
    fn commit_finished_subagents(&mut self) {
        for peer in self.subagents.take_peer_messages() {
            self.add_side_cell(PeerMessageCell::new(
                peer.from,
                peer.to,
                peer.delivery,
                peer.text,
            ));
        }
        for finished in self.subagents.take_finished() {
            if finished.depth > 0 {
                // Nested completions are rendered as compact lines inside their controlling
                // parent's summary. Their full detail is retained by the parent transcript and
                // the picker; adding them to root history would flatten the hierarchy.
                continue;
            }
            let node_id = finished.node_id;
            let card = SubagentSummaryCell::new(finished);
            self.subagents.register_summary(node_id, card.revival());
            self.add_side_cell(card);
        }
    }

    /// Called before every draw: while a child runs, ask for the next animation frame.
    pub(in crate::chatwidget) fn subagent_pre_draw_tick(&mut self) {
        if self.subagents.has_running() {
            let frame = if self.subagents.animations() {
                ANIMATION_FRAME
            } else {
                CLOCK_FRAME
            };
            self.frame_requester.schedule_frame_in(frame);
        }
    }

    pub(crate) fn running_subagents(&self) -> usize {
        self.subagents.running_count()
    }

    /// Whether Esc stops the running subagents: they run on while the agent that started them is
    /// idle, so there is no turn for Esc to interrupt instead.
    pub(crate) fn subagents_stoppable(&self) -> bool {
        self.subagents.has_running() && !self.bottom_pane.is_task_running()
    }

    /// Stop the subagents that run while the agent is idle, as stopping the agent would.
    pub(crate) fn stop_background_subagents(&mut self) {
        self.submit_harness_action(crate::app_event::HarnessAction::StopSubagents);
        self.add_info_message("Stopping the subagents".to_owned(), None);
    }

    /// The suffix the status line carries while children run.
    pub(in crate::chatwidget) fn subagent_status_suffix(&self) -> String {
        match self.subagents_running_shown {
            0 => String::new(),
            1 => " · 1 subagent running".to_owned(),
            count => format!(" · {count} subagents running"),
        }
    }
}
