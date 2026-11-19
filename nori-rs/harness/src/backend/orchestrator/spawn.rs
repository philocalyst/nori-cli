//! Opening an actor's session: for a new delegation, a history fork, or the revival of an actor
//! whose session ended.

use std::sync::Arc;

use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::acp::v1 as acp;

use super::Inner;
use super::child;
use super::delegation::ForkSpec;
use super::directory::ActorId;
use super::directory::Link;
use super::program::ActorScope;

/// How the child's session begins.
enum Start {
    /// An empty session that takes the first task.
    Fresh,
    /// A session continuing a conversation prefix, staged invisibly until its binding is saved.
    Fork {
        fork: ForkSpec,
        parent_view: Option<acp::SessionId>,
    },
    /// The same actor again, with its earlier conversation restored into the new session.
    Revival(ForkSpec),
}

/// Everything needed to open a child's session.
pub(super) struct SpawnPlan {
    spec: crate::runtime::AgentPrepareSpec,
    model: Option<String>,
    options: Vec<(String, String)>,
    start: Start,
    agent: String,
    scope: Option<Arc<ActorScope>>,
}

impl SpawnPlan {
    pub(super) fn fresh(
        spec: crate::runtime::AgentPrepareSpec,
        model: Option<String>,
        options: Vec<(String, String)>,
        fork: Option<ForkSpec>,
        parent_view: Option<acp::SessionId>,
        agent: String,
        scope: Option<Arc<ActorScope>>,
    ) -> Self {
        Self {
            spec,
            model,
            options,
            start: match fork {
                Some(fork) => Start::Fork { fork, parent_view },
                None => Start::Fresh,
            },
            agent,
            scope,
        }
    }
}

/// Whether a revived actor got its earlier conversation back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Restored {
    History,
    Nothing,
    /// Another message restarted the actor first.
    AlreadyStarted,
}

impl Inner {
    /// Open the actor's session in the background. The actor is `Starting` until it is ready, or
    /// ended with the cause when it cannot start; its queued tasks wait.
    pub(super) fn spawn_child(self: &Arc<Self>, actor: &ActorId, plan: SpawnPlan) {
        let orchestrator = Arc::clone(self);
        let actor = actor.clone();
        tokio::spawn(async move {
            let view = actor.session().clone();
            let staged = matches!(plan.start, Start::Fork { .. });
            let role = orchestrator
                .directory()
                .with(|registry| registry.actor(&actor).and_then(|actor| actor.role.clone()));
            let launched = child::launch(
                &orchestrator,
                plan.spec,
                plan.model.as_deref(),
                &plan.options,
                staged,
                view.clone(),
                child::ChildTerms {
                    role,
                    scope: plan.scope.clone(),
                },
            )
            .await;
            let launched = match launched {
                Ok(launched) => launched,
                Err(error) => {
                    orchestrator
                        .actor_gone(
                            &actor,
                            SubagentOutcome::Failed {
                                reason: Some(error),
                            },
                        )
                        .await;
                    return;
                }
            };
            let seeded = match plan.start {
                Start::Fresh => Ok(()),
                Start::Revival(fork) => orchestrator.record_source(&launched, fork).await,
                Start::Fork { fork, parent_view } => {
                    orchestrator
                        .stage_fork(&actor, &launched, fork, parent_view, plan.agent)
                        .await
                }
            };
            if let Err(error) = seeded {
                let _ = launched.child.harness.shutdown().await;
                orchestrator
                    .actor_gone(
                        &actor,
                        SubagentOutcome::Failed {
                            reason: Some(error),
                        },
                    )
                    .await;
                return;
            }
            orchestrator.child_ready(&actor, launched).await;
        });
    }

    async fn record_source(
        &self,
        launched: &child::Launched,
        fork: ForkSpec,
    ) -> Result<(), String> {
        let Some((session_id, start, end)) = fork.source else {
            return Ok(());
        };
        launched
            .child
            .harness
            .record_fork_source(session_id, start, end, fork.context, fork.entries)
            .await
            .map_err(|error| error.to_string())
    }

    /// A history fork is invisible until its binding is durable: record where it came from, save
    /// the binding, attach the user's view, and only then let its events through.
    async fn stage_fork(
        &self,
        actor: &ActorId,
        launched: &child::Launched,
        fork: ForkSpec,
        parent_view: Option<acp::SessionId>,
        agent: String,
    ) -> Result<(), String> {
        let view = actor.session();
        self.record_source(launched, fork).await?;
        let attached = self
            .persist_managed_fork(&launched.child, view, parent_view, agent)
            .await
            .and_then(|()| launched.child.attach(view.clone()));
        if let Err(error) = attached {
            return Err(self.rollback_fork_binding(view, error).await);
        }
        self.announce(actor, SubagentState::Running).await;
        if let Err(error) = launched.child.activate() {
            return Err(self.rollback_fork_binding(view, error).await);
        }
        if let Ok(Some(recorder)) = launched.child.harness.conversation_recorder().await
            && let Ok(events) = recorder.replay_session_events().await
        {
            for event in events {
                if let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    acp::AgentNotification::SessionNotification(mut notification),
                )) = event
                {
                    notification.session_id = view.clone();
                    self.emit(nori_protocol::SessionEvent::Acp(
                        nori_protocol::AcpEvent::ChildNotification(notification),
                    ))
                    .await;
                }
            }
        }
        Ok(())
    }

    async fn rollback_fork_binding(&self, view: &acp::SessionId, error: String) -> String {
        match self.remove_managed_fork_binding(view).await {
            Ok(()) => error,
            Err(rollback) => {
                format!("{error}; failed to remove managed child binding: {rollback}")
            }
        }
    }

    /// The child's session is open: it becomes reachable, and takes the work queued for it.
    async fn child_ready(self: &Arc<Self>, actor: &ActorId, launched: child::Launched) {
        if self.shut_down.load(std::sync::atomic::Ordering::Acquire) {
            let _ = launched.child.harness.shutdown().await;
            return;
        }
        let journal = launched
            .child
            .harness
            .conversation_recorder()
            .await
            .ok()
            .flatten();
        let ready = self.directory().with(|registry| {
            let Some(entry) = registry.actor_mut(actor) else {
                return false;
            };
            if matches!(entry.link, Link::Gone(_)) {
                return false;
            }
            entry.link = Link::Child(launched.child.clone());
            if launched.model.is_some() {
                entry.model.clone_from(&launched.model);
            }
            if let Some(recorder) = &journal {
                entry.journal_ref = Some((
                    recorder.project_id().to_owned(),
                    recorder.session_id().to_owned(),
                ));
                entry.journal.clone_from(&journal);
            }
            registry.note_session(actor, launched.session.clone());
            true
        });
        if !ready {
            let _ = launched.child.harness.shutdown().await;
            return;
        }
        self.directory().touch();
        self.journal_state(actor).await;
        self.announce(actor, SubagentState::Running).await;
        self.pump(actor).await;
    }

    /// Restart an actor whose session ended, with the same alias and identity and, when its
    /// journal was kept, its conversation. The caller queues the message that revives it.
    ///
    /// A managed child restarts under the orchestrator that started it. A provider's own child
    /// is restored as a managed actor on the same agent, seeded with the history recorded for
    /// it; when that history or its configuration was not recorded, the error says so.
    pub(super) async fn revive(&self, actor: &ActorId, address: &str) -> Result<Restored, String> {
        let (kind, owner, provider) = self
            .directory()
            .with(|registry| {
                registry.actor(actor).map(|entry| {
                    (
                        entry.kind,
                        entry.owner.as_ref().and_then(std::sync::Weak::upgrade),
                        entry
                            .native_backend
                            .as_ref()
                            .and_then(std::sync::Weak::upgrade),
                    )
                })
            })
            .ok_or_else(|| "the actor no longer exists".to_owned())?;
        match (kind, owner, provider) {
            (super::directory::ActorKind::Managed, Some(owner), _) => {
                owner.revive_owned(actor).await
            }
            (super::directory::ActorKind::Managed, None, _) => Err(format!(
                "`{address}` cannot be restarted: the session that started it is gone. Delegate \
                 the task again."
            )),
            (super::directory::ActorKind::Native, _, Some(backend)) => backend
                .revive_native_child(actor.session().clone())
                .await
                .map(|()| Restored::History)
                .map_err(|error| {
                    format!(
                        "`{address}` is a provider's own child session that ended, and it could \
                         not be restored as a managed actor: {error:#}. Delegate the task to a \
                         new actor instead."
                    )
                }),
            (super::directory::ActorKind::Native, _, None) => Err(format!(
                "`{address}` is a provider's own child session whose connection is gone; \
                 delegate the task to a new actor instead."
            )),
            (super::directory::ActorKind::Root, _, _) => Err(format!(
                "`{address}` is the root session and is never restarted."
            )),
        }
    }

    /// [`Inner::revive`] on the orchestrator that started the actor.
    async fn revive_owned(self: &Arc<Self>, actor: &ActorId) -> Result<Restored, String> {
        if self.shut_down.load(std::sync::atomic::Ordering::Acquire) {
            return Err("this session is ending".to_owned());
        }
        let taken = self.directory().with(|registry| {
            let entry = registry.actor_mut(actor)?;
            let dead = match &entry.link {
                Link::Gone(_) => true,
                Link::Child(child) => child.is_closed(),
                Link::Root(_) | Link::Starting | Link::Staged(_) | Link::Native { .. } => false,
            };
            if !dead {
                return None;
            }
            entry.link = Link::Starting;
            Some((
                entry.agent.clone(),
                entry.model.clone(),
                entry.options.clone(),
                entry.scope.clone(),
                entry.journal.clone(),
                entry.journal_ref.clone(),
                entry.worktree.clone(),
            ))
        });
        let Some((agent, model, options, scope, journal, journal_ref, worktree)) = taken else {
            return Ok(Restored::AlreadyStarted);
        };
        // After a restart the journal is reopened from where it was written.
        let journal = match (journal, journal_ref) {
            (Some(recorder), _) => Some(recorder),
            (None, Some((project, session))) => {
                let loader =
                    crate::transcript::TranscriptLoader::new(self.seed.config.nori_home.clone());
                async {
                    let transcript = loader.load_transcript(&project, &session).await.ok()?;
                    crate::transcript::TranscriptRecorder::continue_session(
                        loader.session_path(&project, &session),
                        &transcript.meta,
                    )
                    .await
                    .ok()
                    .map(Arc::new)
                }
                .await
            }
            (None, None) => None,
        };
        let history = match &journal {
            Some(recorder) => match recorder.visible_records().await {
                Ok(records) if !records.is_empty() => {
                    let session = recorder.session_id().to_owned();
                    let source = records
                        .first()
                        .zip(records.last())
                        .map(|(first, last)| (session.clone(), first.id, last.id));
                    let entries = crate::transcript::fork_context::replay_entries_from_records(
                        &records, &session,
                    );
                    crate::transcript::fork_context::context_from_records(records)
                        .ok()
                        .map(|context| ForkSpec {
                            context,
                            source,
                            entries,
                        })
                }
                Ok(_) | Err(_) => None,
            },
            None => None,
        };
        let mut config = (*self.seed.config).clone();
        config.active_agent.clone_from(&agent);
        if let Some(path) = &worktree {
            config.cwd.clone_from(path);
        }
        config.approval_policy = self.approval_policy();
        if let Some(model) = &model {
            let remembered = super::catalog::remembered(&self.seed.config.nori_home, &agent).models;
            let value = super::models::find(&remembered, model)
                .map_or_else(|| model.trim().to_owned(), |offered| offered.value.clone());
            config.default_models.insert(agent.clone(), value);
        }
        let restored = if history.is_some() {
            Restored::History
        } else {
            Restored::Nothing
        };
        let spec = crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: self.seed.cli_version.clone(),
            session_context: None,
            initial_context: history.as_ref().map(|fork| fork.context.clone()),
        };
        let plan = SpawnPlan {
            spec,
            model,
            options,
            start: history.map_or(Start::Fresh, Start::Revival),
            agent,
            scope,
        };
        self.announce(actor, SubagentState::Running).await;
        self.spawn_child(actor, plan);
        Ok(restored)
    }
}
