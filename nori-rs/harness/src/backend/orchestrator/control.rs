//! The user's controls over a child, through the directory: send, steer, promote, cancel, and
//! the handle to the child's own control plane.

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

use super::DescendantRoute;
use super::Inner;
use super::Orchestrator;
use super::delegation::Mode;
use super::delivery::Outgoing;
use super::directory::Actor;
use super::directory::ActorId;
use super::directory::ActorKind;
use super::directory::Link;
use super::task::Party;
use super::task::TaskStatus;
use crate::runtime::HarnessHandle;

/// What a view id names, as far as the user's controls care.
enum Control {
    /// A child Nori manages, whose session is open.
    Ready(ActorId, super::child::Child),
    /// A managed child whose session is still opening.
    Starting,
    /// An adoption not yet published, or a session that ended.
    Unavailable(&'static str),
    /// Not a managed child.
    Foreign,
}

fn classify(actor: Option<&Actor>) -> Control {
    let Some(actor) = actor.filter(|actor| actor.kind == ActorKind::Managed) else {
        return Control::Foreign;
    };
    match &actor.link {
        Link::Child(child) => Control::Ready(actor.id.clone(), child.clone()),
        Link::Starting => Control::Starting,
        Link::Staged(_) => Control::Unavailable("the child session is being adopted"),
        Link::Gone(_) => Control::Unavailable("The child session is no longer available"),
        Link::Root(_) | Link::Native { .. } => Control::Foreign,
    }
}

impl Inner {
    /// Show the user's queued prompts for the actor, when they changed.
    pub(super) async fn emit_user_queue(&self, actor: &ActorId) {
        let change = self.directory().with(|registry| {
            let entry = registry.actor_mut(actor)?;
            let prompts: Vec<String> = entry
                .tasks
                .iter()
                .filter(|task| task.status == TaskStatus::Queued && task.requester == Party::User)
                .map(|task| task.messages[0].body.clone())
                .collect();
            if prompts == entry.queue_shown {
                return None;
            }
            entry.queue_shown.clone_from(&prompts);
            Some((entry.wire.clone(), prompts))
        });
        if let Some((session_id, prompts)) = change {
            self.emit(SessionEvent::Nori(NoriEvent::SubagentQueueChanged {
                session_id,
                prompts,
            }))
            .await;
        }
    }

    /// The child behind a view, waiting for a session that is still opening.
    async fn child_of(
        &self,
        view: &acp::SessionId,
    ) -> Result<(ActorId, super::child::Child), String> {
        let mut changed = self.directory().subscribe();
        loop {
            let control = self.directory().with(|registry| {
                classify(registry.by_session(view).and_then(|id| registry.actor(id)))
            });
            match control {
                Control::Ready(id, child) => return Ok((id, child)),
                Control::Starting => {}
                Control::Unavailable(reason) => return Err(reason.to_owned()),
                Control::Foreign => return Err(format!("No controlled child session {view}")),
            }
            changed
                .changed()
                .await
                .map_err(|_| "The child session is no longer available".to_owned())?;
        }
    }
}

impl Orchestrator {
    /// Whether the view is a child Nori manages, published to the user.
    pub(crate) async fn owns_view(&self, session_id: &acp::SessionId) -> bool {
        self.inner.directory().with(|registry| {
            !matches!(
                classify(
                    registry
                        .by_session(session_id)
                        .and_then(|id| registry.actor(id))
                ),
                Control::Foreign | Control::Unavailable("the child session is being adopted")
            )
        })
    }

    /// The managed child an address names (alias, `actor://`, or a session id), as the view id
    /// the user's controls use.
    pub(crate) fn resolve_actor_view(&self, address: &str) -> Option<acp::SessionId> {
        let caller = self.inner.caller().ok()?;
        self.inner.directory().with(|registry| {
            let resolved = registry.resolve(address, &caller).ok()?;
            registry
                .actor(&resolved.actor)
                .map(|actor| actor.wire.clone())
        })
    }

    /// Send a user's follow-up to an orchestrated child, queueing behind its active turn.
    pub(crate) async fn send_user_content_message_locked(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
    ) -> Result<(), String> {
        self.send_user_content(session_id, content, false).await
    }

    pub(crate) async fn steer_user_content_message_locked(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
    ) -> Result<(), String> {
        self.send_user_content(session_id, content, true).await
    }

    async fn send_user_content(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
        steer: bool,
    ) -> Result<(), String> {
        let text = content
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let text = if text.trim().is_empty()
            && content
                .iter()
                .any(|block| !matches!(block, acp::ContentBlock::Text(_)))
        {
            "Attachment".to_owned()
        } else {
            text
        };
        let actor = self
            .inner
            .directory()
            .with(|registry| {
                let id = registry.by_session(&session_id)?;
                // An adoption not yet published takes no work: nothing can start in it yet.
                (!matches!(registry.actor(id)?.link, Link::Staged(_))).then(|| id.clone())
            })
            .ok_or_else(|| format!("No controlled child session {session_id}"))?;
        self.inner
            .send_locked(
                &actor,
                Outgoing {
                    from: Party::User,
                    body: text,
                    content: Some(content),
                    mode: Mode::Background,
                    timeout: None,
                    steer,
                    cc_parent: false,
                },
            )
            .await
            .map(drop)
    }

    /// Run the oldest queued user prompt now, interrupting the turn in progress.
    pub(crate) async fn promote_user_queue(
        &self,
        session_id: &acp::SessionId,
    ) -> Result<(), String> {
        let (actor, _) = self.inner.child_of(session_id).await?;
        let queued = self.inner.directory().with(|registry| {
            registry.actor(&actor).is_some_and(|entry| {
                entry
                    .tasks
                    .iter()
                    .any(|task| task.status == TaskStatus::Queued)
            })
        });
        if !queued {
            return Ok(());
        }
        self.inner.interrupt(&actor).await;
        Ok(())
    }

    pub(crate) async fn cancel_user_turn(&self, session_id: &acp::SessionId) -> Result<(), String> {
        self.inner.child_of(session_id).await?.1.cancel().await
    }

    pub(crate) async fn controlled_subagent_handle(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<HarnessHandle> {
        match self.inner.child_of(session_id).await {
            Ok((_, child)) => Some(child.harness),
            Err(_) => {
                let next = self.descendant_next_hop(session_id).await?;
                next.handle
                    .controlled_subagent_handle(next.session_id)
                    .await
                    .ok()
                    .flatten()
            }
        }
    }

    pub(crate) async fn descendant_next_hop(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<DescendantRoute> {
        self.inner
            .directory()
            .with(|registry| registry.routes.get(session_id).cloned())
    }
}
