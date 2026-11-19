//! The one resolver: every address form an agent, a program or the user can use for an actor.

use super::Actor;
use super::ActorId;
use super::Link;
use super::Registry;
use super::normalize_alias;
use crate::backend::orchestrator::task::Party;
use crate::backend::orchestrator::task::TaskId;

/// Who is asking. Task ids are private to the actor that requested them.
#[derive(Debug, Clone)]
pub(in crate::backend::orchestrator) struct Caller {
    pub(in crate::backend::orchestrator) id: ActorId,
}

/// What an address named.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::backend::orchestrator) struct Resolved {
    pub(in crate::backend::orchestrator) actor: ActorId,
    /// The task, when the address was a `task-N`.
    pub(in crate::backend::orchestrator) task: Option<TaskId>,
}

/// An address that names nothing, with what does exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::backend::orchestrator) struct ResolveError(String);

impl std::fmt::Display for ResolveError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl From<ResolveError> for String {
    fn from(error: ResolveError) -> Self {
        error.0
    }
}

/// The address forms, as the agents are told them.
pub(in crate::backend::orchestrator) const ADDRESS_FORMS: &str = "`task-N` (a task you started), an alias such as `reviewer`, `actor://reviewer`, or `actor://<session-id>`";

impl Registry {
    /// Actors an address may name: every registered actor except an adoption not yet published.
    pub(in crate::backend::orchestrator) fn addressable(&self) -> impl Iterator<Item = &Actor> {
        self.order
            .iter()
            .filter_map(|id| self.actors.get(id))
            .filter(|actor| !matches!(actor.link, Link::Staged(_)))
    }

    /// Resolve `raw` for `caller`.
    pub(in crate::backend::orchestrator) fn resolve(
        &self,
        raw: &str,
        caller: &Caller,
    ) -> Result<Resolved, ResolveError> {
        let trimmed = raw.trim();
        if let Some(task) = TaskId::from_address(trimmed) {
            return match self.task(&task) {
                Some((actor, found))
                    if found.requester == Party::Actor(caller.id.clone())
                        && !matches!(actor.link, Link::Staged(_)) =>
                {
                    Ok(Resolved {
                        actor: actor.id.clone(),
                        task: Some(task),
                    })
                }
                Some(_) | None => Err(self.not_found(trimmed, caller)),
            };
        }
        let key = normalize_alias(trimmed);
        if key.is_empty() {
            return Err(self.not_found(trimmed, caller));
        }
        let hit = self
            .aliases
            .get(key)
            .or_else(|| {
                self.by_session
                    .get(&nori_protocol::acp::v1::SessionId::new(key))
            })
            .or_else(|| self.unique_alias_ignoring_case(key));
        match hit.and_then(|id| self.actors.get(id)) {
            Some(actor) if !matches!(actor.link, Link::Staged(_)) => Ok(Resolved {
                actor: actor.id.clone(),
                task: None,
            }),
            Some(_) | None => Err(self.not_found(trimmed, caller)),
        }
    }

    fn unique_alias_ignoring_case(&self, key: &str) -> Option<&ActorId> {
        let mut matches = self
            .aliases
            .iter()
            .filter(|(alias, _)| alias.eq_ignore_ascii_case(key))
            .map(|(_, id)| id);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }

    fn not_found(&self, raw: &str, caller: &Caller) -> ResolveError {
        let mut text = format!("`{raw}` does not name a task of yours or an actor.");
        text.push_str(&self.listing(caller));
        ResolveError(text)
    }

    /// What can be addressed: the actors with their state, the caller's own tasks, and the forms.
    pub(in crate::backend::orchestrator) fn listing(&self, caller: &Caller) -> String {
        let actors: Vec<String> = self
            .addressable()
            .filter(|actor| actor.id != caller.id)
            .map(|actor| {
                let agent = if actor.agent.is_empty() {
                    String::new()
                } else {
                    format!("{}, ", actor.agent)
                };
                format!(
                    "`{}` ({agent}{})",
                    actor.address(),
                    actor.presence().label()
                )
            })
            .collect();
        let me = Party::Actor(caller.id.clone());
        let tasks: Vec<String> = self
            .tasks_requested_by(&me)
            .map(|(actor, task)| {
                format!("{} ({}, {})", task.id, actor.address(), task.status.label())
            })
            .collect();
        let mut text = if actors.is_empty() {
            " There are no other actors yet.".to_owned()
        } else {
            format!(" Actors you can address: {}.", actors.join(", "))
        };
        if !tasks.is_empty() {
            text.push_str(&format!(" Your tasks: {}.", tasks.join(", ")));
        }
        text.push_str(&format!(" Address forms: {ADDRESS_FORMS}."));
        text
    }
}
