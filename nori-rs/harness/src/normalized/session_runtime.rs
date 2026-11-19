use std::collections::BTreeMap;
use std::collections::VecDeque;

use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::AgentCommandInfo;
use super::turn::Detachment;
pub use super::turn::SessionPhaseView;
use super::turn::Turn;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsageState {
    pub used_tokens: i64,
    pub total_tokens: i64,
    pub cost_display: Option<String>,
}

/// A user prompt waiting to be sent to ACP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedPromptKind {
    User,
    Compact,
    /// A `/compact` forwarded to an agent that advertises a native `compact`
    /// slash command. Sent as an ordinary in-session turn: no summarize-and-swap
    /// and no summary re-injection.
    NativeCompact,
    GoalContinuation,
    /// Results of delegated tasks, handed to the agent once it is idle.
    TaskResults,
    /// Events of the monitors the agent armed, handed to it once it is idle.
    MonitorEvents,
    /// A prompt the session's workflow hooks to what the last turn did.
    WorkflowHook,
}

/// A user prompt waiting to be sent to ACP.
#[derive(Debug, Clone)]
pub struct QueuedPrompt {
    pub event_id: String,
    pub kind: QueuedPromptKind,
    pub text: String,
    /// Prompt-level ACP metadata forwarded to the downstream agent.
    pub prompt_meta: Option<acp::Meta>,
    /// Exact caller-supplied ACP content, before Nori prepends private context.
    pub original_content: Vec<acp::ContentBlock>,
    /// Content sent to the downstream agent, including any Nori context.
    pub content: Vec<acp::ContentBlock>,
    pub display_text: Option<String>,
}

/// The single runtime object per ACP session. Everything else is derived from it.
#[derive(Debug, Clone, Default)]
pub struct SessionRuntime {
    /// The one unit of agent work in progress, or `None` when the session is idle.
    pub turn: Option<Turn>,
    /// Set when a local request ended; the next activity with no turn continues it.
    pub detached: Option<Detachment>,
    /// The sequence number the next turn gets.
    pub next_turn_seq: i64,
    pub queue: VecDeque<QueuedPrompt>,
    /// Prompts typed ahead before the session ended, restored from its journal. They are not
    /// queued: nothing starts them but the user, who sends the oldest or drops one.
    pub held: VecDeque<QueuedPrompt>,
    /// A session swap is in flight: no prompt may start until it is released.
    pub fenced: bool,
    /// Permission requests declined since their notice was last published, by tool title.
    pub declines: BTreeMap<String, i64>,
    /// Slash commands the agent currently advertises.
    pub available_commands: Vec<AgentCommandInfo>,
}

impl SessionRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Flattened view of the current phase.
    pub fn phase_view(&self) -> SessionPhaseView {
        SessionPhaseView::from(self.turn.as_ref())
    }
}
