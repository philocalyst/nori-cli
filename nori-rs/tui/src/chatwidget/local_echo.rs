//! Optimistic echo of a submitted prompt.
//!
//! The harness echoes every user prompt as `UserMessageChunk`s once the `session/prompt` request is
//! on the wire, each chunk carrying the prompt's [`PromptEchoId`]. Waiting for that echo makes the
//! user's own text appear after the round trip through the session runtime, so when the session is
//! idle the widget paints the prompt immediately and then drops the chunks that carry the id of a
//! prompt it painted. Recognizing the echo is one hash lookup and never compares text.

use std::collections::HashSet;

use nori_protocol::PromptEchoId;
use nori_protocol::acp::v1 as acp;

use super::*;

/// What the harness echo of an image attachment renders as.
const IMAGE_ATTACHMENT_ECHO: &str = " [image attachment]";

/// Ids of the prompts submitted from this widget, and which of them are already on screen.
#[derive(Debug, Default)]
pub(super) struct PendingEchoes {
    painted: HashSet<PromptEchoId>,
    issued: i64,
}

impl PendingEchoes {
    /// A fresh id for a prompt about to be submitted.
    pub(super) fn issue(&mut self) -> PromptEchoId {
        self.issued += 1;
        PromptEchoId::new(format!("nori-tui-{}-{}", std::process::id(), self.issued))
    }

    pub(super) fn clear(&mut self) {
        self.painted.clear();
    }

    #[cfg(test)]
    pub(super) fn painted_ids(&self) -> impl Iterator<Item = &PromptEchoId> {
        self.painted.iter()
    }

    /// Whether `chunk` echoes a prompt that is already painted.
    fn is_painted(&self, chunk: &acp::ContentChunk) -> bool {
        PromptEchoId::in_meta(chunk.meta.as_ref())
            .is_some_and(|id| self.painted.contains(&PromptEchoId::new(id)))
    }
}

impl ChatWidget {
    /// Whether a prompt submitted now starts a turn immediately, so painting it
    /// locally cannot get ahead of the queue.
    pub(super) fn can_echo_locally(&self) -> bool {
        self.replay_source.is_none()
            && !self.bottom_pane.is_task_running()
            && !self.bottom_pane.has_queued_user_messages()
            && !self.proactive_turn_active
            && self.owned_prompt_request_id.is_none()
            && matches!(
                self.acp_session_phase,
                None | Some(crate::presentation::session_runtime::SessionPhaseView::Idle)
            )
    }

    /// Paint the user's prompt as the active cell and show the working status
    /// without waiting for the harness.
    ///
    /// `painted_already` is true when [`Self::paint_prompt_cell`] ran before the
    /// session existed, so only the turn state and the pending echo are set.
    pub(super) fn echo_submitted_prompt(
        &mut self,
        text: &str,
        image_count: usize,
        painted_already: bool,
        echo: PromptEchoId,
    ) {
        if !painted_already {
            self.paint_prompt_cell(shown_prompt(text, image_count));
        }
        self.pending_echoes.painted.insert(echo);
        // The harness' `Prompt` phase event that follows continues this turn
        // instead of restarting it.
        self.acp_session_phase =
            Some(crate::presentation::session_runtime::SessionPhaseView::Prompt);
        self.on_task_started();
    }

    /// Show the prompt as the active cell.
    fn paint_prompt_cell(&mut self, shown: String) {
        self.flush_active_cell();
        self.session_stats.record_user_message();
        self.active_cell
            .set(Some(Box::new(history_cell::new_user_prompt(shown))));
        self.needs_final_message_separator = false;
        self.request_redraw();
    }

    /// Paint a prompt entered before its session exists. It is submitted, and
    /// its echo registered, when the session starts.
    pub(super) fn paint_initial_prompt(&mut self) {
        let Some(message) = &self.initial_user_message else {
            return;
        };
        let shown = shown_prompt(&message.text, message.image_paths.len());
        self.paint_prompt_cell(shown);
        self.initial_echo_painted = true;
    }

    /// Whether a live update is the harness echo of a prompt already painted locally.
    pub(super) fn is_painted_echo(&self, update: &acp::SessionUpdate) -> bool {
        self.replay_source.is_none()
            && matches!(update, acp::SessionUpdate::UserMessageChunk(chunk) if self.pending_echoes.is_painted(chunk))
    }

    /// The `session/prompt` request could not be handed to the harness: undo the
    /// optimistic turn state and show the error under the prompt.
    pub(crate) fn on_prompt_submit_failed(&mut self, message: String) {
        self.pending_echoes.clear();
        if self.owned_prompt_request_id.is_none()
            && self.acp_session_phase
                == Some(crate::presentation::session_runtime::SessionPhaseView::Prompt)
        {
            self.acp_session_phase =
                Some(crate::presentation::session_runtime::SessionPhaseView::Idle);
            self.bottom_pane.set_task_running(false);
            self.bottom_pane.set_interrupt_hint_visible(false);
        }
        self.add_error_message(message);
    }
}

/// The prompt as the harness echo renders it.
fn shown_prompt(text: &str, image_count: usize) -> String {
    let mut shown = text.to_owned();
    for _ in 0..image_count {
        shown.push_str(IMAGE_ATTACHMENT_ECHO);
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn chunk(text: &str, echo: Option<&PromptEchoId>) -> acp::ContentChunk {
        acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)))
            .meta(echo.map(PromptEchoId::to_meta))
    }

    #[test]
    fn chunks_of_a_painted_prompt_are_recognized_by_id_whatever_their_text() {
        let mut echoes = PendingEchoes::default();
        let mine = echoes.issue();
        echoes.painted.insert(mine.clone());
        assert_eq!(echoes.is_painted(&chunk("hello", Some(&mine))), true);
        // The harness may split or rewrite the text; the id still matches.
        assert_eq!(
            echoes.is_painted(&chunk(" [image attachment]", Some(&mine))),
            true
        );
        assert_eq!(echoes.is_painted(&chunk("hello", None)), false);
    }

    #[test]
    fn a_prompt_that_was_not_painted_renders_from_its_echo() {
        let mut echoes = PendingEchoes::default();
        let painted = echoes.issue();
        let queued = echoes.issue();
        assert_ne!(painted, queued);
        echoes.painted.insert(painted);
        assert_eq!(echoes.is_painted(&chunk("queued", Some(&queued))), false);
        echoes.clear();
        assert_eq!(echoes.painted.is_empty(), true);
    }
}
