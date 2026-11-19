//! Replaying a session's history into the scrollback.
//!
//! A replayed update goes through the same handlers as a live one, so a resumed session renders
//! exactly as it did live: text that goes on across a tool call stays one message, an update for
//! a tool that is already shown adds nothing, and the separator before an answer appears where it
//! appeared live. What a replay does not have is the turn lifecycle (the prompt and idle phases,
//! the prompt's response) that tells a live session when a turn ends and what that does to the
//! history. A replay stands in for it: a user message starts a turn, which ends the turn before
//! it, and the end of the replay ends the last one.
//!
//! With no turn running there is no timer either. A "Worked for" separator takes its duration from
//! the update it precedes, which carries how long its turn had been running when it was first
//! observed (`nori_protocol::turn_elapsed_in_meta`).

use super::*;

impl ChatWidget {
    /// Handle a normalized update of a replay, as a live one is handled.
    pub(super) fn handle_replay_client_event(&mut self, event: crate::presentation::ClientEvent) {
        if let crate::presentation::ClientEvent::MessageDelta(delta) = &event
            && delta.stream == crate::presentation::MessageStream::User
            && !delta.delta.is_empty()
        {
            let continues_active_message = self.active_user_message_id == delta.message_id
                && self
                    .active_cell
                    .as_ref()
                    .is_some_and(|cell| cell.as_any().is::<history_cell::UserHistoryCell>());
            if !continues_active_message {
                self.end_replayed_turn();
            }
        }
        self.handle_client_event(event);
    }

    /// Note what a replayed update says about its turn before it is handled.
    pub(super) fn note_replayed_update(&mut self, update: &nori_protocol::acp::v1::SessionUpdate) {
        if let nori_protocol::acp::v1::SessionUpdate::AgentMessageChunk(chunk) = update
            && let Some(elapsed) = nori_protocol::turn_elapsed_in_meta(chunk.meta.as_ref())
        {
            self.replay_turn_elapsed = Some(elapsed);
        }
    }

    /// End the replayed turn: what the end of a live turn does to the history, and nothing of what
    /// it does to the rest of the screen.
    pub(super) fn end_replayed_turn(&mut self) {
        self.flush_answer_stream_with_separator();
        self.pending_client_tool_cells.clear();
        self.finalize_active_cell_as_failed();
        self.settle_client_tool_calls();
        self.full_reasoning_buffer.clear();
        self.reasoning_buffer.clear();
        self.replay_turn_elapsed = None;
    }
}
