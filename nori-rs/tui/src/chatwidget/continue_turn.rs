//! `/continue`: taking up a turn that ended before it finished.

use nori_protocol::acp::v1::StopReason;

use super::ChatWidget;
use crate::history_cell::StopWhy;
use crate::history_cell::StoppedTurnCell;

/// What `/continue` asks of an agent that had started on the prompt.
const CONTINUATION: &str = "Continue where you left off.";

/// The last turn, when it ended interrupted, failed, or cut short by a limit.
pub(crate) struct Unfinished {
    prompt: String,
    /// The agent had answered (text, reasoning, a tool call) before it stopped.
    reached_agent: bool,
}

impl ChatWidget {
    /// A prompt the user sent starts the turn `/continue` would take up.
    pub(super) fn note_turn_prompt(&mut self, prompt: &str) {
        self.turn_prompt = Some(prompt.to_owned());
        self.turn_reached_agent = false;
        self.unfinished = None;
    }

    pub(super) fn note_agent_output(&mut self) {
        self.turn_reached_agent = true;
    }

    /// The turn ended with `stop_reason`, `failed` or not. One that did not finish can be
    /// continued, and the user is told so.
    pub(super) fn note_turn_end(&mut self, stop_reason: StopReason, failed: bool) {
        let prompt = self.turn_prompt.take();
        // A turn the person did not start (a compaction, delivered results) says its own failure;
        // the bar is for what they asked, or stopped.
        let theirs = prompt.is_some() || stop_reason == StopReason::Cancelled;
        let finished = !failed
            && !matches!(
                stop_reason,
                StopReason::Cancelled | StopReason::MaxTokens | StopReason::MaxTurnRequests
            );
        self.unfinished = prompt.filter(|_| !finished).map(|prompt| Unfinished {
            prompt,
            reached_agent: self.turn_reached_agent,
        });
        if !finished && theirs {
            let reason = if failed {
                StopWhy::Other
            } else {
                match stop_reason {
                    StopReason::Cancelled => StopWhy::Interrupted,
                    StopReason::MaxTokens => StopWhy::TokenLimit,
                    StopReason::MaxTurnRequests => StopWhy::TurnLimit,
                    // EndTurn and Refusal end a turn on their own terms, and the enum is
                    // non_exhaustive, so any later stop reason lands here too.
                    _ => StopWhy::Other,
                }
            };
            self.add_to_history(StoppedTurnCell {
                reason,
                can_continue: self.unfinished.is_some(),
            });
        }
        if finished && self.config.git.auto_commit {
            self.app_event_tx
                .send(crate::app_event::AppEvent::CommitChanges { automatic: true });
        }
    }

    /// Send the unfinished turn's prompt again if the agent never answered it, or ask the agent
    /// to go on with it.
    pub(crate) fn continue_unfinished_turn(&mut self) {
        let Some(unfinished) = self.unfinished.take() else {
            self.add_info_message(
                "Nothing to continue: the last turn finished.".to_owned(),
                None,
            );
            return;
        };
        let text = if unfinished.reached_agent {
            CONTINUATION.to_owned()
        } else {
            unfinished.prompt
        };
        self.submit_user_message_text(text);
    }
}
