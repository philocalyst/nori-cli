//! A child session: a full harness session on the agent a task names, driven one task at a time.
//!
//! Each child has an actor that owns its event stream. The actor runs a task's turn, relays what
//! the child does into the parent's stream under the task's view id, asks the user for the child's
//! permissions through the parent, and reports the turn's end with the child's final message.

use tokio::sync::mpsc;
use tokio::sync::oneshot;

use super::task::TaskId;
use crate::runtime::HarnessHandle;
use nori_protocol::acp::v1 as acp;

mod actor;
mod final_text;
mod launch;
mod phase;
mod relay;

use actor::Active;
use final_text::FinalText;
pub(super) use launch::ChildTerms;
pub(super) use launch::launch;
pub(super) use relay::scoped_id;

/// One turn the actor is asked to run.
pub(super) struct Turn {
    pub(super) task: TaskId,
    /// The session id the user's view shows the actor's activity under.
    pub(super) view_id: acp::SessionId,
    pub(super) prompt: String,
    /// Original typed content for user follow-ups; other turns use the text prompt.
    pub(super) content: Option<Vec<acp::ContentBlock>>,
    /// Where the actor reports what the child is doing.
    pub(super) progress: super::task::SharedProgress,
}

/// The orchestrator's side of a running child. The actor stops when the last one is dropped.
#[derive(Clone)]
pub(super) struct Child {
    pub(super) harness: HarnessHandle,
    turns: mpsc::UnboundedSender<Command>,
}

enum Command {
    Attach(acp::SessionId),
    Activate,
    Run(Turn),
    Cancel(oneshot::Sender<Result<(), String>>),
}

impl Child {
    /// The child's actor has stopped: its session is gone, whatever else says so.
    pub(super) fn is_closed(&self) -> bool {
        self.turns.is_closed()
    }

    /// Associate an idle adopted session with its public view before admitting turns.
    pub(super) fn attach(&self, view: acp::SessionId) -> Result<(), String> {
        self.turns
            .send(Command::Attach(view))
            .map_err(|_| "the child session has ended".to_owned())
    }

    pub(super) fn activate(&self) -> Result<(), String> {
        self.turns
            .send(Command::Activate)
            .map_err(|_| "the child session has ended".to_owned())
    }

    /// Hand the actor a turn. Fails only when the child is gone.
    pub(super) fn run(&self, turn: Turn) -> Result<(), String> {
        self.turns
            .send(Command::Run(turn))
            .map_err(|_| "the child session has ended".to_owned())
    }

    /// Interrupt the turn in progress; the actor ends it silently, as the requester's or the
    /// user's choice.
    pub(super) async fn cancel(&self) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.turns
            .send(Command::Cancel(tx))
            .map_err(|_| "the child session has ended".to_owned())?;
        rx.await
            .map_err(|_| "the child session has ended".to_owned())?
    }
}

/// What starting a child found out about it.
pub(super) struct Launched {
    pub(super) child: Child,
    /// The model the child runs, as the agent names it.
    pub(super) model: Option<String>,
    /// The id of the ACP session behind the actor.
    pub(super) session: acp::SessionId,
}
