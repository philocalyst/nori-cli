//! The harness side of a program run: actors, replies and what their own recovery did, through
//! the backend's actor controls. Session-level effects are in [`super::effects`].
//!
//! An actor is a managed child with a session of its own, and recovery is that session's: its
//! engine retries and moves it between routes (models, agents, the ranked catalog) under the
//! policy plus whatever the program says for the turn. The host passes the program's overrides
//! down with each message, reports back what the engine did, and fails the turn only with
//! what the engine could not recover.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicI64;
use std::time::Duration;

use nori_conduit::ActorRef;
use nori_conduit::ActorSpec;
use nori_conduit::ConduitError;
use nori_conduit::Delivery;
use nori_conduit::ProgramHost;
use nori_conduit::Receipt;
use nori_conduit::RecoveryNote;
use nori_conduit::Reply;
use nori_conduit::SandboxPolicy;
use nori_conduit::ToolOutcome;
use nori_conduit::WorldScope;
use nori_conduit::schemas::Isolation;
use nori_workflow::Recovery;
use nori_workflow::RecoveryOverride;
use nori_workflow::RecoveryPolicy;
use tokio::sync::Mutex;

use crate::backend::AcpBackend;
use crate::backend::classify_text;
use crate::backend::orchestrator::ActorScope;
use crate::backend::orchestrator::ProgramActor;
use crate::transcript::TranscriptRecorder;

/// How long one wait on a task lasts before the host asks again; a reply is waited for until
/// it comes, the run is cancelled or the step's own timeout fires.
const WAIT_SLICE: Duration = Duration::from_secs(600);

#[derive(Default)]
struct ActorSlot {
    /// Standing instructions, delivered ahead of the first message the actor receives.
    briefing: Option<String>,
}

pub(super) struct HarnessProgramHost {
    pub(super) usage: super::usage::UsageMeter,
    pub(super) backend: Arc<AcpBackend>,
    pub(super) recorder: Arc<TranscriptRecorder>,
    recovery: RecoveryPolicy,
    actors: Mutex<HashMap<String, ActorSlot>>,
    /// The conversation generation the program is allowed to act in; a successful rewind by
    /// the program itself moves it forward.
    pub(super) generation: AtomicI64,
}

impl HarnessProgramHost {
    pub(super) fn new(
        backend: Arc<AcpBackend>,
        recorder: Arc<TranscriptRecorder>,
        recovery: RecoveryPolicy,
        generation: i64,
    ) -> Self {
        Self {
            usage: super::usage::UsageMeter::start(),
            backend,
            recorder,
            recovery,
            actors: Mutex::new(HashMap::new()),
            generation: AtomicI64::new(generation),
        }
    }
}

/// A failure of the machinery around an actor (it does not exist, the conversation was
/// rewound): not something a retry or another route changes.
fn control_error(agent: &str, error: impl std::fmt::Display) -> ConduitError {
    ConduitError::ChildFailed {
        agent_id: agent.to_owned(),
        reason: error.to_string(),
    }
}

/// A failed turn of an actor, classified the way the session's own failures are.
fn turn_failure(message: &str) -> ConduitError {
    let class = classify_text(message);
    ConduitError::Host {
        class: class.kind(),
        message: message.to_owned(),
        retry_after_ms: class
            .retry_after()
            .map(|wait| i64::try_from(wait.as_millis()).unwrap_or(i64::MAX)),
    }
}

/// The actor's own words: a task result starts with `[Route: ...]` when its session changed
/// route during the turn, which is the run's journal's to say, not the reply's.
fn strip_route_note(text: String) -> String {
    match text
        .strip_prefix("[Route: ")
        .and_then(|rest| rest.split_once("].\n"))
    {
        Some((_, reply)) => reply.to_owned(),
        None => text,
    }
}

#[nori_conduit::async_trait]
impl ProgramHost for HarnessProgramHost {
    fn workspace_root(&self) -> &Path {
        self.backend.program_cwd()
    }

    fn base_recovery(&self, agent: &str, role: Option<&str>) -> Recovery {
        self.recovery.resolve(agent, role)
    }

    async fn spawn_actor(&self, spec: &ActorSpec) -> Result<ActorRef, ConduitError> {
        let actor = &spec.actor;
        let worktree = actor.isolation != Isolation::Shared
            || spec
                .world
                .as_ref()
                .is_some_and(|world| world.isolation != Isolation::Shared);
        let view = self
            .backend
            .program_actor_spawn(ProgramActor {
                alias: spec.alias.clone(),
                agent: actor.agent.clone(),
                model: actor.model.clone(),
                role: actor.role.clone(),
                options: actor.options.clone().into_iter().collect(),
                worktree,
                scope: ActorScope {
                    tools: (!spec.tools.is_empty()).then(|| spec.tools.clone()),
                    instructions: spec.skill_instructions.clone(),
                    sandbox: spec.sandbox.clone(),
                    home: spec.world.as_ref().and_then(|world| world.home.clone()),
                },
            })
            .await
            .map_err(|error| control_error(&actor.agent, error))?;
        self.actors.lock().await.insert(
            spec.alias.clone(),
            ActorSlot {
                briefing: actor
                    .instructions
                    .clone()
                    .filter(|text| !text.trim().is_empty()),
            },
        );
        Ok(ActorRef {
            alias: spec.alias.clone(),
            session: view.session_id,
            agent: view.agent,
            model: view.model,
        })
    }

    async fn resolve_actor(&self, reference: &str) -> Result<ActorRef, ConduitError> {
        let view =
            self.backend
                .actor_resolve(reference)
                .map_err(|error| ConduitError::UnknownActor {
                    alias: reference.to_owned(),
                    known: error.to_string(),
                })?;
        Ok(ActorRef {
            alias: view.actor,
            session: view.session_id,
            agent: view.agent,
            model: view.model,
        })
    }

    async fn send(
        &self,
        actor: &ActorRef,
        message: &str,
        delivery: Delivery,
        recovery: &RecoveryOverride,
    ) -> Result<Receipt, ConduitError> {
        let briefing = self
            .actors
            .lock()
            .await
            .get_mut(&actor.alias)
            .and_then(|slot| slot.briefing.take());
        let body = match briefing {
            Some(briefing) => format!("{briefing}\n\n{message}"),
            None => message.to_owned(),
        };
        // The actor's own session applies this for the turn it is about to run.
        self.backend
            .program_set_recovery(&actor.session, recovery.clone());
        let snapshot = self
            .backend
            .actor_send(&actor.alias, body, delivery == Delivery::Steer)
            .await
            .map_err(|error| control_error(&actor.agent, error))?;
        let id = snapshot
            .task_id
            .ok_or_else(|| control_error(&actor.agent, "the message did not start a task"))?
            .to_string();
        let receipt = Receipt {
            actor: actor.alias.clone(),
            turn: 0,
            id,
        };
        self.mark_usage(&actor.session, &receipt.id).await;
        Ok(receipt)
    }

    async fn drain_recovery(&self, actor: &ActorRef) -> Vec<RecoveryNote> {
        self.backend
            .program_recovery_notes(&actor.session)
            .into_iter()
            .map(|note| match note {
                crate::backend::orchestrator::RecoveryNote::Retry(retry) => RecoveryNote::Retry {
                    attempt: retry.attempt,
                    max_attempts: retry.max_attempts,
                    wait_ms: retry.wait_ms,
                    class: retry.class.name().to_owned(),
                    detail: retry.detail,
                },
                crate::backend::orchestrator::RecoveryNote::Route(changed) => {
                    RecoveryNote::RouteChanged {
                        from: changed.from.to_string(),
                        to_agent: changed.to.agent.clone(),
                        to_model: changed.to.model.clone(),
                        class: match changed.cause {
                            nori_workflow::RouteCause::Failure(kind) => kind.name().to_owned(),
                            nori_workflow::RouteCause::PrimaryRestored => {
                                "primary_restored".to_owned()
                            }
                        },
                        step: changed.step.index,
                    }
                }
            })
            .collect()
    }

    async fn await_reply(
        &self,
        receipt: &Receipt,
        timeout: Option<Duration>,
    ) -> Result<Reply, ConduitError> {
        let started = std::time::Instant::now();
        loop {
            let snapshot = self
                .backend
                .actor_await(&receipt.id, WAIT_SLICE)
                .await
                .map_err(|error| control_error(&receipt.actor, error))?;
            match snapshot.status {
                "completed" => {
                    let text = strip_route_note(snapshot.result.unwrap_or_default());
                    // What the actor's agent reported spending on the turn; when it reported
                    // nothing, a guess of four characters a token, labelled as one.
                    let reported = self.reported_since(receipt).await;
                    let (tokens, tokens_estimated) = match reported {
                        Some(tokens) => (tokens, false),
                        None => (i64::try_from(text.len() / 4).unwrap_or(0), true),
                    };
                    return Ok(Reply {
                        text,
                        tokens,
                        tokens_estimated,
                    });
                }
                "failed" => {
                    return Err(turn_failure(
                        &snapshot
                            .error
                            .unwrap_or_else(|| "the actor's turn failed".to_owned()),
                    ));
                }
                "cancelled" => {
                    return Err(control_error(
                        &receipt.actor,
                        format!("the turn `{}` was cancelled", receipt.id),
                    ));
                }
                "incomplete" => {
                    return Err(control_error(
                        &receipt.actor,
                        format!(
                            "the turn `{}` ended before the actor finished ({})",
                            receipt.id,
                            snapshot.stop_reason.unwrap_or("no stop reason")
                        ),
                    ));
                }
                _ => {
                    if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                        return Err(ConduitError::Timeout {
                            ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
                        });
                    }
                }
            }
        }
    }

    async fn cancel(&self, receipt: &Receipt) -> Result<(), ConduitError> {
        self.backend
            .actor_cancel(&receipt.id)
            .await
            .map_err(|error| control_error(&receipt.actor, error))
    }

    async fn call_tool(
        &self,
        name: &str,
        args: &nori_conduit::ConduitValue,
        sandbox: Option<&SandboxPolicy>,
    ) -> Result<ToolOutcome, ConduitError> {
        self.host_tool(name, args, sandbox).await
    }

    async fn prompt(&self, text: &str) -> Result<(), ConduitError> {
        self.submit_prompt(text).await
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<(), ConduitError> {
        self.apply_config(key, value).await
    }

    async fn select_context(&self, start: i64, end: i64) -> Result<String, ConduitError> {
        self.context_range(start, end).await
    }

    async fn fork(&self, source: &str, start: i64, end: i64) -> Result<(), ConduitError> {
        self.fork_range(source, start, end).await
    }

    async fn rewind(&self, head: Option<i64>) -> Result<(), ConduitError> {
        self.rewind_conversation(head).await
    }

    async fn exec(
        &self,
        program: &str,
        args: &[String],
        cwd: Option<&str>,
    ) -> Result<ToolOutcome, ConduitError> {
        self.run_process(program, args, cwd).await
    }

    async fn enter_world(&self, world: &WorldScope) -> Result<(), ConduitError> {
        if let Some(home) = &world.home {
            let parent = self.backend.program_nori_home().to_path_buf();
            let home = home.clone();
            tokio::task::spawn_blocking(move || super::home::seed(&parent, &home))
                .await
                .map_err(|error| ConduitError::Runtime {
                    message: format!("seeding the home of world `{}`: {error}", world.name),
                })?
                .map_err(|error| ConduitError::Runtime {
                    message: format!("cannot seed the home of world `{}`: {error}", world.name),
                })?;
        }
        Ok(())
    }
}
