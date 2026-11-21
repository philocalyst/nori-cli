//! The turn ledger's data model.
//!
//! A session has at most one [`Turn`] at a time. A turn is a span of agent work with
//! exactly one owner, and everything the agent does while it is open (message text, tool calls,
//! permission requests) is accounted to it. See `docs/turn-lifecycle.md`.

use std::collections::BTreeSet;

use nori_protocol::AgentTurnCause;
use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::message_assembly::MessageAssembly;
use super::session_runtime::QueuedPrompt;

/// Who started the work a turn accounts for.
#[derive(Debug, Clone)]
pub enum TurnOwner {
    /// Our `session/prompt`. It ends with its response or a failure.
    Local {
        request_id: acp::RequestId,
        prompt: Box<QueuedPrompt>,
        /// `session/cancel` has been sent and the response has not arrived.
        cancelling: bool,
    },
    /// Our `session/load`. It ends with its response.
    Replay { request_id: acp::RequestId },
    /// The agent is working without a request of ours. It ends when the agent reports `idle`,
    /// when it goes quiet, or when the user abandons it.
    Agent {
        cause: AgentTurnCause,
        /// `session/cancel` has been sent and the agent has not gone quiet.
        cancel_sent: bool,
    },
}

/// A tool call a turn has seen, with the last status the agent reported for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackedTool {
    pub call_id: String,
    pub status: acp::ToolCallStatus,
    /// It runs a subagent (Claude's `Agent`), which ends if the turn is cancelled.
    pub subagent: bool,
}

impl TrackedTool {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status,
            acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
        )
    }
}

/// Everything accounted to one turn.
#[derive(Debug, Clone, Default)]
pub struct TurnWork {
    pub messages: MessageAssembly,
    /// Tool calls in the order the turn first saw them.
    pub tools: Vec<TrackedTool>,
    /// The latest tool calls, for loop detection.
    pub loops: crate::backend::turn::loops::CallWindow,
    /// Permission and question requests the agent is waiting on, by wire request ID.
    pub pending_requests: BTreeSet<String>,
    /// Counts agent activity so the quiet window can tell whether anything happened during it.
    pub activity: i64,
    /// A quiet-window timer is running for this turn.
    pub quiet_timer_armed: bool,
    /// Start the front of the queue as soon as this turn ends, whatever way it ends.
    pub start_next_on_end: bool,
    /// The user cancelled the turn this one continues and the agent kept working. A cancel the
    /// agent ignored is no longer a reason to decline what it asks: the user decides.
    pub cancel_ignored: bool,
    /// The activity count the silence check last saw.
    pub silent_seen_activity: i64,
    /// Consecutive silence checks that saw no activity.
    pub silent_ticks: i64,
    /// `AgentSilent` has been published and not yet cleared.
    pub silent_published: bool,
}

impl TurnWork {
    /// Record the status the agent reported for a tool call, tracking it if it is new, and
    /// whether it runs a subagent once the agent says so.
    pub fn track_tool(
        &mut self,
        call_id: &str,
        status: Option<acp::ToolCallStatus>,
        subagent: bool,
    ) {
        match self.tools.iter_mut().find(|tool| tool.call_id == call_id) {
            Some(tool) => {
                if let Some(status) = status {
                    tool.status = status;
                }
                tool.subagent |= subagent;
            }
            None => self.tools.push(TrackedTool {
                call_id: call_id.to_owned(),
                status: status.unwrap_or(acp::ToolCallStatus::Pending),
                subagent,
            }),
        }
    }

    /// A subagent runs inside the turn: cancelling it would end the subagent's work.
    pub fn runs_subagent(&self) -> bool {
        self.tools
            .iter()
            .any(|tool| tool.subagent && !tool.is_terminal())
    }

    /// Nothing the agent is waiting on or running is left open.
    pub fn is_quiescent(&self) -> bool {
        self.pending_requests.is_empty() && self.tools.iter().all(TrackedTool::is_terminal)
    }
}

/// The session's single unit of agent work.
#[derive(Debug, Clone)]
pub struct Turn {
    /// Identity within the session. Unlike a request ID it never changes while the turn is open
    /// and is never reused.
    pub seq: i64,
    pub owner: TurnOwner,
    pub work: TurnWork,
}

impl Turn {
    /// The request that owns the turn, when one does.
    pub fn request_id(&self) -> Option<&acp::RequestId> {
        match &self.owner {
            TurnOwner::Local { request_id, .. } | TurnOwner::Replay { request_id } => {
                Some(request_id)
            }
            TurnOwner::Agent { .. } => None,
        }
    }

    /// The user cancelled this turn and it has not ended yet.
    pub fn is_cancelling(&self) -> bool {
        match &self.owner {
            TurnOwner::Local { cancelling, .. } => *cancelling,
            TurnOwner::Agent { cancel_sent, .. } => *cancel_sent,
            TurnOwner::Replay { .. } => false,
        }
    }

    /// The phase published to clients.
    pub fn phase(&self) -> nori_protocol::SessionPhase {
        match &self.owner {
            TurnOwner::Local {
                request_id,
                cancelling: false,
                ..
            } => nori_protocol::SessionPhase::Prompting {
                request_id: request_id.clone(),
            },
            TurnOwner::Local {
                request_id,
                cancelling: true,
                ..
            } => nori_protocol::SessionPhase::Cancelling {
                request_id: request_id.clone(),
            },
            TurnOwner::Replay { request_id } => nori_protocol::SessionPhase::Loading {
                request_id: request_id.clone(),
            },
            TurnOwner::Agent { cause, cancel_sent } => nori_protocol::SessionPhase::Agent {
                cause: *cause,
                cancelling: *cancel_sent,
            },
        }
    }
}

/// A turn ended while the agent may still be working on it. Activity that follows soon
/// after is an `Agent` turn with cause `Detached`, and it inherits whether the user had cancelled
/// the turn that ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detachment {
    /// The turn that ended.
    pub turn_seq: i64,
    /// The agent kept working after `session/cancel` and Nori had to end the turn without it.
    pub cancelled: bool,
}

/// Flattened phase for consumers that need no request IDs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhaseView {
    Idle,
    Loading,
    Prompt,
    Cancelling,
    /// The agent is working without a request of ours.
    Agent,
}

impl SessionPhaseView {
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Loading => "loading",
            Self::Prompt => "prompt",
            Self::Cancelling => "cancelling",
            Self::Agent => "agent",
        }
    }
}

impl From<Option<&Turn>> for SessionPhaseView {
    fn from(turn: Option<&Turn>) -> Self {
        let Some(turn) = turn else {
            return Self::Idle;
        };
        match &turn.owner {
            TurnOwner::Replay { .. } => Self::Loading,
            TurnOwner::Local {
                cancelling: true, ..
            } => Self::Cancelling,
            TurnOwner::Local {
                cancelling: false, ..
            } => Self::Prompt,
            TurnOwner::Agent { .. } => Self::Agent,
        }
    }
}
