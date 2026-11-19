//! Stage and adopt an independent harness for a native child view.
//!
//! The caller owns native-session quiescence and must keep its generation gate held through
//! validation and commit. Staging never attaches the public view or sends a prompt.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use nori_protocol::acp::v1 as acp;

use super::Orchestrator;
use super::child;
use super::delegation::Delegation;
use super::delegation::ForkSpec;
use super::directory::ActorId;
use super::directory::ActorKind;
use super::directory::Link;
use super::directory::NewActor;
use super::task::Party;
use crate::runtime::HarnessHandle;
use crate::transcript::TranscriptLine;

#[derive(Clone)]
pub(crate) struct NativeRebindSpec {
    pub(crate) public_id: acp::SessionId,
    pub(crate) parent_id: acp::SessionId,
    pub(crate) agent: String,
    pub(crate) model: Option<String>,
    pub(crate) options: Vec<(String, String)>,
    pub(crate) context: String,
    pub(crate) source: (String, i64, i64),
    pub(crate) entries: Vec<TranscriptLine>,
    pub(crate) nested_bindings: Vec<(String, String)>,
    pub(crate) external_managed: Vec<acp::SessionId>,
}

/// An isolated session, not yet reachable through the parent. Dropping it closes its actor.
pub(crate) struct StagedNativeRebind {
    public_id: acp::SessionId,
    parent_id: acp::SessionId,
    agent: String,
    agent_name: String,
    generation: i64,
    launched: child::Launched,
}

impl StagedNativeRebind {
    /// Inspect the isolated journal and session before publishing the view mapping.
    pub(crate) fn handle(&self) -> HarnessHandle {
        self.launched.child.harness.clone()
    }
}

impl Orchestrator {
    /// Fork a native child's selected prefix using its own provider configuration.
    #[expect(
        clippy::too_many_arguments,
        reason = "the source, view, and provider configuration must stay explicit"
    )]
    pub(crate) async fn fork_native_history(
        &self,
        parent_view: acp::SessionId,
        agent: String,
        model: Option<String>,
        options: Vec<(String, String)>,
        context: String,
        source: (String, i64, i64),
        entries: Vec<TranscriptLine>,
    ) -> Result<(), String> {
        self.inner
            .delegate(Delegation {
                agent: Some(agent),
                model,
                title: Some("History fork".to_owned()),
                options,
                fork: Some(ForkSpec {
                    context,
                    source: Some(source),
                    entries,
                }),
                parent_view: Some(parent_view),
                requester: Some(Party::User),
                ..Delegation::new("Continue from the conversation above.")
            })
            .await
            .map(drop)
    }

    pub(crate) async fn prepare_native_rebind(
        &self,
        mut spec: NativeRebindSpec,
    ) -> Result<StagedNativeRebind, String> {
        if spec.source.0.trim().is_empty()
            || spec.source.1 < 0
            || spec.source.2 < spec.source.1
            || spec.public_id == spec.parent_id
        {
            return Err("the selected child history range is invalid".to_owned());
        }
        // Managed fork tasks and their descendants have their own owner; do not
        // reinterpret them as descendants of the adopted provider session.
        let mut managed: std::collections::HashSet<_> = spec
            .external_managed
            .iter()
            .filter(|id| *id != &spec.public_id)
            .cloned()
            .collect();
        let owned_managed: Vec<acp::SessionId> = spec
            .nested_bindings
            .iter()
            .filter_map(|(name, _)| {
                name.strip_prefix("managed-child-journal:")
                    .map(acp::SessionId::new)
            })
            .collect();
        loop {
            let before = managed.len();
            for entry in &spec.entries {
                if let crate::transcript::TranscriptEntry::SessionEvent(event) = &entry.entry
                    && let nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                        nori_protocol::SubagentWire::Update(update),
                    )) = &event.event
                    && ((update.session_id.to_string().starts_with("nori-task-")
                        && !spec.nested_bindings.iter().any(|(name, _)| {
                            name == &format!("managed-child-journal:{}", update.session_id)
                        }))
                        || managed.contains(&update.parent_session_id)
                        || managed.iter().chain(owned_managed.iter()).any(
                            |scope: &acp::SessionId| {
                                update.session_id.to_string().starts_with(&format!(
                                    "nori-relay:{}:{scope}",
                                    scope.to_string().len()
                                ))
                            },
                        ))
                {
                    managed.insert(update.session_id.clone());
                }
            }
            if managed.len() == before {
                break;
            }
        }
        spec.entries.retain(|entry| {
            let crate::transcript::TranscriptEntry::SessionEvent(event) = &entry.entry else {
                return true;
            };
            match &event.event {
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Update(update),
                )) => update.session_id != spec.public_id && !managed.contains(&update.session_id),
                nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Message(message),
                )) => {
                    !message
                        .sender_session_id
                        .as_ref()
                        .is_some_and(|id| managed.contains(id))
                        && !message
                            .recipient_session_id
                            .as_ref()
                            .is_some_and(|id| managed.contains(id))
                }
                nori_protocol::SessionEvent::Nori(
                    nori_protocol::NoriEvent::SubagentQueueChanged { session_id, .. },
                ) => !managed.contains(session_id),
                nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(
                    notification,
                )) => !managed.contains(&notification.session_id),
                _ => true,
            }
        });
        for entry in &spec.entries {
            if let crate::transcript::TranscriptEntry::SessionEvent(event) = &entry.entry
                && let nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Update(update),
                )) = &event.event
                && !spec.nested_bindings.iter().any(|(name, _)| {
                    name == &format!("native-child-journal:{}", update.session_id)
                        || name == &format!("managed-child-journal:{}", update.session_id)
                })
            {
                return Err(format!(
                    "the selected history for {} contains descendant {} without a durable provider binding (managed owners: {:?})",
                    spec.public_id, update.session_id, spec.external_managed
                ));
            }
        }
        let generation = self.inner.generation.load(Ordering::Acquire);
        if self.inner.shut_down.load(Ordering::Acquire)
            || self.inner.directory().with(|registry| {
                // A provider child's own entry is the one being taken over; anything else with
                // this view is already someone's.
                registry
                    .by_session(&spec.public_id)
                    .and_then(|id| registry.actor(id))
                    .is_some_and(|actor| actor.kind != ActorKind::Native)
                    || registry.routes.contains_key(&spec.public_id)
            })
        {
            return Err("the child view is unavailable for adoption".to_owned());
        }
        let agent_name = crate::registry::list_available_agents()
            .into_iter()
            .find(|candidate| candidate.agent_name == spec.agent)
            .map_or_else(|| spec.agent.clone(), |candidate| candidate.display_name);
        let mut config = (*self.inner.seed.config).clone();
        config.active_agent = spec.agent.clone();
        config.approval_policy = self.inner.approval_policy();
        let normalized_model = spec
            .model
            .as_deref()
            .map(str::trim)
            .filter(|model| !model.is_empty() && *model != "default")
            .map(str::to_owned);
        if let Some(model) = &normalized_model {
            let offered = super::catalog::remembered(&config.nori_home, &spec.agent).models;
            let value = super::models::find(&offered, model)
                .map_or_else(|| model.trim().to_owned(), |model| model.value.clone());
            config.default_models.insert(spec.agent.clone(), value);
        }
        let prepare = crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: self.inner.seed.cli_version.clone(),
            session_context: None,
            initial_context: Some(spec.context.clone()),
        };
        let launched = child::launch(
            &self.inner,
            prepare,
            normalized_model.as_deref(),
            &spec.options,
            true,
            spec.public_id.clone(),
            child::ChildTerms::default(),
        )
        .await?;
        let (session_id, start, end) = spec.source;
        if let Err(error) = launched
            .child
            .harness
            .record_fork_source(session_id, start, end, spec.context, spec.entries)
            .await
        {
            let _ = launched.child.harness.shutdown().await;
            return Err(format!(
                "could not record the selected child history: {error:#}"
            ));
        }
        match launched.child.harness.conversation_recorder().await {
            Ok(Some(recorder)) => {
                for (name, payload) in &spec.nested_bindings {
                    if let Err(error) = recorder.record_command(name, payload.clone()).await {
                        let _ = launched.child.harness.shutdown().await;
                        return Err(format!(
                            "could not copy the selected child binding: {error:#}"
                        ));
                    }
                }
                if let Err(error) = recorder.replay_session_events().await {
                    let _ = launched.child.harness.shutdown().await;
                    return Err(format!(
                        "could not validate the staged child journal: {error:#}"
                    ));
                }
            }
            Ok(None) => {
                let _ = launched.child.harness.shutdown().await;
                return Err("the staged child has no conversation journal".to_owned());
            }
            Err(error) => {
                let _ = launched.child.harness.shutdown().await;
                return Err(format!(
                    "could not open the staged child journal: {error:#}"
                ));
            }
        }
        if !spec.nested_bindings.is_empty()
            && let Err(error) = launched
                .child
                .harness
                .restore_seeded_native_children()
                .await
        {
            let _ = launched.child.harness.shutdown().await;
            return Err(format!(
                "could not restore selected descendant sessions: {error:#}"
            ));
        }
        Ok(StagedNativeRebind {
            public_id: spec.public_id,
            parent_id: spec.parent_id,
            agent: spec.agent,
            agent_name,
            generation,
            launched,
        })
    }

    /// Reserve the view mapping but keep the staged actor silent until activation.
    /// The caller must hold its native quiescence and per-child routing guard through activation.
    pub(crate) async fn commit_native_rebind(
        &self,
        staged: StagedNativeRebind,
    ) -> Result<HarnessHandle, String> {
        let child = staged.launched.child.clone();
        let handle = child.harness.clone();
        let journal = handle.conversation_recorder().await.ok().flatten();
        let view_id = staged.public_id.clone();
        let owner = Arc::downgrade(&self.inner);
        let committed = self.inner.directory().with(|registry| {
            if self.inner.shut_down.load(Ordering::Acquire)
                || staged.generation != self.inner.generation.load(Ordering::Acquire)
            {
                return Err("the conversation changed before the child was adopted".to_owned());
            }
            let existing = registry.by_session(&view_id).cloned();
            let taking_over = existing
                .as_ref()
                .and_then(|id| registry.actor(id))
                .is_some_and(|actor| actor.kind == ActorKind::Native);
            if (existing.is_some() && !taking_over) || registry.routes.contains_key(&view_id) {
                return Err("the child view is already controlled by another session".to_owned());
            }
            let parent = registry
                .by_session(&staged.parent_id)
                .cloned()
                .or_else(|| registry.root.clone());
            let id = match existing.filter(|_| taking_over) {
                // The provider's child becomes a managed one under the same identity: its
                // aliases, parent and task history stay.
                Some(id) => {
                    if let Some(actor) = registry.actor_mut(&id) {
                        actor.kind = ActorKind::Managed;
                        actor.link = Link::Staged(child.clone());
                        actor.owner = Some(owner);
                        actor.announce_parent = Some(staged.parent_id.clone());
                        actor.agent.clone_from(&staged.agent);
                        actor.agent_name.clone_from(&staged.agent_name);
                        if staged.launched.model.is_some() {
                            actor.model.clone_from(&staged.launched.model);
                        }
                        if actor.parent.is_none() {
                            actor.parent = parent;
                        }
                    }
                    id
                }
                None => {
                    let id = ActorId::new(view_id.clone());
                    registry.register(NewActor {
                        parent,
                        id: id.clone(),
                        kind: ActorKind::Managed,
                        announce_parent: Some(staged.parent_id.clone()),
                        alias: None,
                        agent: staged.agent.clone(),
                        agent_name: staged.agent_name.clone(),
                        model: staged.launched.model.clone(),
                        options: Vec::new(),
                        role: None,
                        scope: None,
                        title: "Continued child session".to_owned(),
                        worktree: None,
                        link: Link::Staged(child.clone()),
                        owner: Some(owner),
                    });
                    id
                }
            };
            if let Some(actor) = registry.actor_mut(&id) {
                actor.journal = journal;
            }
            // Attach has no await: a failure is rolled back before any reader can take the lock.
            if let Err(error) = child.attach(view_id.clone()) {
                restore_or_remove(registry, &id);
                return Err(error);
            }
            Ok(())
        });
        committed?;
        self.inner.directory().touch();
        Ok(handle)
    }

    /// Release buffered events only after the durable binding was persisted.
    pub(crate) async fn activate_native_rebind(&self, view: &acp::SessionId) -> Result<(), String> {
        let activated = self.inner.directory().with(|registry| {
            let id = registry
                .by_session(view)
                .cloned()
                .ok_or_else(|| "the child view has no pending adoption".to_owned())?;
            let actor = registry
                .actor_mut(&id)
                .ok_or_else(|| "the staged child is unavailable".to_owned())?;
            let Link::Staged(child) = &actor.link else {
                return Err("the child view has no pending adoption".to_owned());
            };
            let child = child.clone();
            child.activate()?;
            actor.link = Link::Child(child);
            Ok(id)
        });
        self.inner.directory().touch();
        let id = activated?;
        // Whatever was queued while the adoption was unpublished starts now.
        self.inner.pump(&id).await;
        Ok(())
    }

    /// Discard a reserved, not-yet-activated mapping after a failed durable binding.
    pub(crate) async fn rollback_native_rebind(&self, view: &acp::SessionId) -> Result<(), String> {
        let (id, child) = self.inner.directory().with(|registry| {
            let id = registry
                .by_session(view)
                .cloned()
                .ok_or_else(|| "the child view has no pending adoption".to_owned())?;
            let actor = registry
                .actor(&id)
                .ok_or_else(|| "the staged child is unavailable".to_owned())?;
            let Link::Staged(child) = &actor.link else {
                return Err("the child view has no pending adoption".to_owned());
            };
            if actor.open_tasks().next().is_some() {
                return Err("the staged child has already admitted work".to_owned());
            }
            Ok((id, child.clone()))
        })?;
        child
            .harness
            .shutdown()
            .await
            .map_err(|error| format!("could not stop the staged child: {error:#}"))?;
        self.inner
            .directory()
            .with(|registry| restore_or_remove(registry, &id));
        self.inner.directory().touch();
        Ok(())
    }
}

/// Undo a reserved adoption: a provider child that was being taken over goes back to being one
/// (the provider's next report corrects its state); a new view is forgotten.
fn restore_or_remove(registry: &mut super::directory::Registry, id: &ActorId) {
    let provider = registry
        .actor(id)
        .and_then(|actor| actor.native_backend.clone());
    match (provider, registry.actor_mut(id)) {
        (Some(backend), Some(actor)) => {
            actor.kind = ActorKind::Native;
            actor.link = Link::Native {
                backend,
                running: false,
            };
            actor.owner = None;
        }
        (None, _) | (Some(_), None) => registry.remove_actor(id),
    }
}
