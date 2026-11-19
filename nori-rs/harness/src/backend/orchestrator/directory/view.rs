//! How actors are reported to the agents that list them.

use serde::Serialize;

use super::Actor;
use super::ActorId;
use super::ActorKind;
use super::Registry;
use crate::backend::orchestrator::task::TaskStatus;

/// One actor as `task_list` and the tool results show it.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ActorView {
    /// The address to use: the alias when there is one, else the session id.
    pub(crate) actor: String,
    pub(crate) session_id: String,
    pub(crate) kind: &'static str,
    pub(crate) agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    /// `starting`, `idle`, `working`, `ended`, `failed` or `cancelled`. An ended actor is
    /// restarted, with its history, by sending it a message.
    pub(crate) state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent: Option<String>,
    pub(crate) title: String,
    /// Messages sent to the actor that no turn of it has read yet.
    #[serde(skip_serializing_if = "is_zero")]
    pub(crate) queued_messages: i64,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
fn is_zero(number: &i64) -> bool {
    *number == 0
}

impl Actor {
    pub(in crate::backend::orchestrator) fn view(&self, registry: &Registry) -> ActorView {
        ActorView {
            actor: self.address(),
            session_id: self.wire.to_string(),
            kind: match self.kind {
                ActorKind::Root => "root",
                ActorKind::Managed => "child",
                ActorKind::Native => "provider child",
            },
            agent: self.agent.clone(),
            model: self.model.clone(),
            state: self.presence().label(),
            parent: self.parent.as_ref().map(|parent| registry.label(parent)),
            title: self.title.clone(),
            queued_messages: i64::try_from(
                self.tasks
                    .iter()
                    .filter(|task| task.status == TaskStatus::Queued)
                    .map(|task| task.messages.len())
                    .sum::<usize>(),
            )
            .unwrap_or(i64::MAX),
        }
    }
}

impl Registry {
    /// Every addressable actor except `except`, in the order they started.
    pub(in crate::backend::orchestrator) fn views(&self, except: &ActorId) -> Vec<ActorView> {
        self.addressable()
            .filter(|actor| &actor.id != except)
            .map(|actor| actor.view(self))
            .collect()
    }
}
