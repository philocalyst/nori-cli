//! The one host trait a program run needs, plus the signal bus and interrupt registry.
//!
//! Everything a program can do to the world goes through [`ProgramHost`]: starting and
//! addressing actors, delivering messages and collecting replies, switching an actor's
//! route, calling tools and the session-level effects (prompt, config, context, fork, rewind,
//! processes). The harness implements it over its actor controls; [`LocalHost`] is the
//! deterministic in-memory implementation used to validate and dry-run programs.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use async_trait::async_trait;
use nori_workflow::Recovery;
use nori_workflow::RecoveryOverride;
use nori_workflow::RecoveryPolicy;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio::sync::oneshot;

use crate::error::ConduitError;
use crate::schemas::Actor;
use crate::schemas::Isolation;
use crate::schemas::SandboxPolicy;
use crate::value::ConduitValue;

/// Result of a host tool or process invocation, including the token cost charged to budgets.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutcome {
    pub output: ConduitValue,
    pub tokens: i64,
}

/// A running actor, addressed by alias.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    pub alias: String,
    /// The host's identity for the actor's session.
    pub session: String,
    pub agent: String,
    pub model: Option<String>,
}

/// What the host needs to start an actor.
#[derive(Clone, Debug)]
pub struct ActorSpec {
    pub alias: String,
    pub actor: Actor,
    /// The Nori host tools the actor is given: its own `tools` and those of its skills.
    /// Empty leaves the session's tool policy as it is.
    pub tools: Vec<String>,
    /// What the actor's skills tell it about its tools.
    pub skill_instructions: Vec<String>,
    /// What the actor's Nori `bash` and host tools may do: the sandbox of the scope it starts
    /// in, narrowed by its own. `None` restricts nothing.
    pub sandbox: Option<SandboxPolicy>,
    /// The world the actor lives in, when it is not the session's own.
    pub world: Option<WorldScope>,
}

/// One turn of work handed to an actor; the key to its reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub actor: String,
    pub turn: i64,
    /// The host's identity for the turn (a task id, a message id).
    pub id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    pub text: String,
    pub tokens: i64,
    /// Whether `tokens` is a guess from the text's length because the host heard no usage
    /// report for the turn.
    pub tokens_estimated: bool,
}

/// Something the actor's own recovery did while a turn ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecoveryNote {
    /// The failed request will be tried again on the same route.
    Retry {
        attempt: i64,
        max_attempts: i64,
        wait_ms: i64,
        class: String,
        detail: Option<String>,
    },
    /// The actor moved to another route, possibly on another agent.
    RouteChanged {
        from: String,
        to_agent: String,
        to_model: Option<String>,
        class: String,
        step: i64,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Delivered when the actor reaches its next turn boundary.
    Queue,
    /// The actor's current turn is interrupted, then the message is delivered.
    Steer,
}

/// A world a run (or part of one) lives in. The runner creates its directories under the run
/// directory; the host applies them to the actors it starts there.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldScope {
    pub name: String,
    pub isolation: Isolation,
    /// The world's working tree: a worktree for `worktree` and `sandbox` isolation.
    pub root: PathBuf,
    /// A Nori home scoped to the world, when its definition asks for one.
    pub home: Option<PathBuf>,
    pub sandbox: SandboxPolicy,
    pub budget_tokens: Option<i64>,
}

/// Everything a program can do to the world.
#[async_trait]
pub trait ProgramHost: Send + Sync {
    fn workspace_root(&self) -> &Path;

    /// Policy recovery in force for work sent to `agent` in `role`, before program, actor and
    /// step overrides apply.
    fn base_recovery(&self, agent: &str, role: Option<&str>) -> Recovery {
        RecoveryPolicy::default().resolve(agent, role)
    }

    /// Start an actor and return its address.
    async fn spawn_actor(&self, spec: &ActorSpec) -> Result<ActorRef, ConduitError>;

    /// Address an actor the program did not start (`actor://alias`).
    async fn resolve_actor(&self, reference: &str) -> Result<ActorRef, ConduitError> {
        Err(ConduitError::UnknownActor {
            alias: reference.to_owned(),
            known: "none; this host cannot address actors the program did not start".to_owned(),
        })
    }

    /// Hand `message` to an actor as one turn of work. Returns once it is queued or delivered;
    /// the reply is collected with [`ProgramHost::await_reply`].
    ///
    /// `recovery` is what the program, the actor and the enclosing steps say about retrying
    /// and falling back. The host lays it over the policy in force for the actor and the
    /// actor's own session applies it: its recovery engine retries, moves the actor to other
    /// routes (other models, other agents, the ranked catalog) and only then fails the turn.
    /// The run does not recover a second time.
    async fn send(
        &self,
        actor: &ActorRef,
        message: &str,
        delivery: Delivery,
        recovery: &RecoveryOverride,
    ) -> Result<Receipt, ConduitError>;

    /// What the actor's own recovery did since this was last asked (retries it scheduled,
    /// routes it moved to), oldest first, for the run's journal.
    async fn drain_recovery(&self, _actor: &ActorRef) -> Vec<RecoveryNote> {
        Vec::new()
    }

    /// Wait for the turn to end and return the actor's final message. Fails with
    /// [`ConduitError::Host`] carrying the failure kind when the actor's request failed.
    async fn await_reply(
        &self,
        receipt: &Receipt,
        timeout: Option<Duration>,
    ) -> Result<Reply, ConduitError>;

    /// Stop waiting for, and stop, a turn (a race lost, a step timed out).
    async fn cancel(&self, _receipt: &Receipt) -> Result<(), ConduitError> {
        Ok(())
    }

    /// Run a host tool (bash, git, delegation, ...) under the active sandbox.
    async fn call_tool(
        &self,
        name: &str,
        args: &ConduitValue,
        sandbox: Option<&SandboxPolicy>,
    ) -> Result<ToolOutcome, ConduitError>;

    /// Queue a prompt in the session that runs the program.
    async fn prompt(&self, _text: &str) -> Result<(), ConduitError> {
        unsupported(
            "prompting the running session",
            "this host has no session to prompt",
        )
    }

    /// Apply an advertised ACP configuration option to the running session.
    async fn set_config(&self, _key: &str, _value: &str) -> Result<(), ConduitError> {
        unsupported(
            "session configuration",
            "this host has no session to configure",
        )
    }

    /// Canonical context for an inclusive range of the conversation's visible events.
    async fn select_context(&self, _start: i64, _end: i64) -> Result<String, ConduitError> {
        unsupported("context selection", "this host has no conversation journal")
    }

    async fn fork(&self, _source: &str, _start: i64, _end: i64) -> Result<(), ConduitError> {
        unsupported("forking history", "this host has no conversation journal")
    }

    async fn rewind(&self, _head: Option<i64>) -> Result<(), ConduitError> {
        unsupported("rewinding", "this host has no conversation journal")
    }

    /// Run an external process: only `Unsafe` steps reach this.
    async fn exec(
        &self,
        _program: &str,
        _args: &[String],
        _cwd: Option<&str>,
    ) -> Result<ToolOutcome, ConduitError> {
        unsupported("running processes", "this host never starts processes")
    }

    /// A world was opened; apply its isolation to actors started in it.
    async fn enter_world(&self, _world: &WorldScope) -> Result<(), ConduitError> {
        Ok(())
    }

    async fn leave_world(&self, _world: &WorldScope) -> Result<(), ConduitError> {
        Ok(())
    }
}

fn unsupported<T>(what: &str, reason: &str) -> Result<T, ConduitError> {
    Err(ConduitError::Unsupported {
        what: what.to_owned(),
        reason: reason.to_owned(),
    })
}

type ToolFn =
    Arc<dyn Fn(&ConduitValue) -> Result<ToolOutcome, ConduitError> + Send + Sync + 'static>;
type ReplyFn = Arc<dyn Fn(&str, i64) -> Result<String, ConduitError> + Send + Sync + 'static>;

/// Deterministic in-memory [`ProgramHost`]: actors answer from scripted functions (or with a
/// plain acknowledgement), tools are registered closures, nothing leaves the process. Used to
/// validate and dry-run programs, and by tests.
pub struct LocalHost {
    workspace_root: PathBuf,
    state: StdMutex<LocalState>,
}

#[derive(Default)]
struct LocalState {
    actors: BTreeMap<String, ActorRef>,
    scripts: HashMap<String, ReplyFn>,
    turns: HashMap<String, i64>,
    pending: HashMap<String, Result<Reply, ConduitError>>,
    notes: HashMap<String, Vec<RecoveryNote>>,
    tools: HashMap<String, ToolFn>,
    log: Vec<String>,
}

impl Default for LocalHost {
    fn default() -> Self {
        Self::new(PathBuf::from("/workspace"))
    }
}

impl LocalHost {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        let host = Self {
            workspace_root: workspace_root.into(),
            state: StdMutex::new(LocalState::default()),
        };
        host.register_tool("echo", |args| {
            Ok(ToolOutcome {
                output: ConduitValue::String(args.display_compact()),
                tokens: 1,
            })
        });
        host
    }

    fn state(&self) -> std::sync::MutexGuard<'_, LocalState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn register_tool<F>(&self, name: impl Into<String>, handler: F)
    where
        F: Fn(&ConduitValue) -> Result<ToolOutcome, ConduitError> + Send + Sync + 'static,
    {
        self.state().tools.insert(name.into(), Arc::new(handler));
    }

    /// Script an actor's replies: `(message, turn)` to the reply text or a failure.
    pub fn script_actor<F>(&self, alias: impl Into<String>, reply: F)
    where
        F: Fn(&str, i64) -> Result<String, ConduitError> + Send + Sync + 'static,
    {
        self.state().scripts.insert(alias.into(), Arc::new(reply));
    }

    /// Everything the host was asked to do, in order.
    pub fn log(&self) -> Vec<String> {
        self.state().log.clone()
    }
}

#[async_trait]
impl ProgramHost for LocalHost {
    fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    async fn spawn_actor(&self, spec: &ActorSpec) -> Result<ActorRef, ConduitError> {
        let actor = ActorRef {
            alias: spec.alias.clone(),
            session: format!("local-{}", spec.alias),
            agent: spec.actor.agent.clone(),
            model: spec.actor.model.clone(),
        };
        let mut state = self.state();
        state.log.push(format!(
            "spawn {} -> {}{}",
            spec.alias,
            actor.agent,
            actor
                .model
                .as_ref()
                .map_or(String::new(), |m| format!(" · {m}"))
        ));
        state.actors.insert(spec.alias.clone(), actor.clone());
        Ok(actor)
    }

    async fn resolve_actor(&self, reference: &str) -> Result<ActorRef, ConduitError> {
        let alias = reference.strip_prefix("actor://").unwrap_or(reference);
        let state = self.state();
        state
            .actors
            .get(alias)
            .cloned()
            .ok_or_else(|| ConduitError::UnknownActor {
                alias: alias.to_owned(),
                known: state.actors.keys().cloned().collect::<Vec<_>>().join(", "),
            })
    }

    async fn send(
        &self,
        actor: &ActorRef,
        message: &str,
        delivery: Delivery,
        recovery: &RecoveryOverride,
    ) -> Result<Receipt, ConduitError> {
        let mut state = self.state();
        let turn = {
            let counter = state.turns.entry(actor.alias.clone()).or_insert(0);
            *counter += 1;
            *counter
        };
        let mut current = state
            .actors
            .get(&actor.alias)
            .cloned()
            .unwrap_or_else(|| actor.clone());
        state.log.push(format!(
            "send {} ({:?}, turn {turn}, on {}): {message}",
            actor.alias, delivery, current.agent
        ));
        // Stands in for the harness's recovery engine: the same policy types, applied the same
        // way (retry the route while the kind is in `retry.on`, then the chain), with no waiting.
        let mut policy = self.base_recovery(&current.agent, None);
        recovery.apply(&mut policy);
        let (mut retries, mut moves) = (0, 0);
        let outcome = loop {
            let attempt = match state.scripts.get(&actor.alias).cloned() {
                Some(script) => script(message, turn),
                None => Ok(format!(
                    "[{}] acknowledged turn {turn}: {}",
                    actor.alias,
                    message.chars().take(60).collect::<String>()
                )),
            };
            let Err(error) = attempt else {
                break attempt;
            };
            let Some(kind) = error.failure_kind() else {
                break Err(error);
            };
            if policy.retry.on.contains(&kind) && retries < policy.retry.max_attempts {
                retries += 1;
                state
                    .notes
                    .entry(actor.alias.clone())
                    .or_default()
                    .push(RecoveryNote::Retry {
                        attempt: retries,
                        max_attempts: policy.retry.max_attempts,
                        wait_ms: 0,
                        class: kind.name().to_owned(),
                        detail: Some(error.to_string()),
                    });
                continue;
            }
            if policy.fallback.on.contains(&kind)
                && let Some(route) = policy.fallback.chain.get(moves)
            {
                moves += 1;
                retries = 0;
                let from = current.agent.clone();
                current = ActorRef {
                    agent: route.agent.clone().unwrap_or_else(|| current.agent.clone()),
                    model: route.model.clone().or_else(|| current.model.clone()),
                    ..current
                };
                state
                    .log
                    .push(format!("route {} -> {}", actor.alias, current.agent));
                state.actors.insert(actor.alias.clone(), current.clone());
                state.notes.entry(actor.alias.clone()).or_default().push(
                    RecoveryNote::RouteChanged {
                        from,
                        to_agent: current.agent.clone(),
                        to_model: current.model.clone(),
                        class: kind.name().to_owned(),
                        step: i64::try_from(moves).unwrap_or(i64::MAX),
                    },
                );
                continue;
            }
            break Err(error);
        }
        .map(|text| Reply {
            text,
            tokens: 10,
            tokens_estimated: true,
        });
        let id = format!("local-{}-{turn}", actor.alias);
        state.pending.insert(id.clone(), outcome);
        Ok(Receipt {
            actor: actor.alias.clone(),
            turn,
            id,
        })
    }

    async fn drain_recovery(&self, actor: &ActorRef) -> Vec<RecoveryNote> {
        self.state().notes.remove(&actor.alias).unwrap_or_default()
    }

    async fn await_reply(
        &self,
        receipt: &Receipt,
        _timeout: Option<Duration>,
    ) -> Result<Reply, ConduitError> {
        self.state().pending.remove(&receipt.id).unwrap_or_else(|| {
            Err(ConduitError::Runtime {
                message: format!(
                    "no pending turn `{}`; a reply is collected once",
                    receipt.id
                ),
            })
        })
    }

    async fn call_tool(
        &self,
        name: &str,
        args: &ConduitValue,
        _sandbox: Option<&SandboxPolicy>,
    ) -> Result<ToolOutcome, ConduitError> {
        let handler = self.state().tools.get(name).cloned();
        match handler {
            Some(handler) => handler(args),
            None => Err(ConduitError::UnknownTool {
                name: name.to_owned(),
            }),
        }
    }

    async fn prompt(&self, text: &str) -> Result<(), ConduitError> {
        self.state().log.push(format!("prompt: {text}"));
        Ok(())
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<(), ConduitError> {
        self.state().log.push(format!("config {key} = {value}"));
        Ok(())
    }
}

/// Topic-based signal bus supporting causal replay of buffered signals and live wakeup.
#[derive(Default)]
pub struct SignalHub {
    state: StdMutex<SignalState>,
}

#[derive(Default)]
struct SignalState {
    channels: HashMap<String, broadcast::Sender<ConduitValue>>,
    latest: HashMap<String, ConduitValue>,
}

impl SignalHub {
    pub fn emit(&self, topic: &str, payload: ConduitValue) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.latest.insert(topic.to_owned(), payload.clone());
        let tx = state
            .channels
            .entry(topic.to_owned())
            .or_insert_with(|| broadcast::channel(64).0);
        let _ = tx.send(payload);
    }

    pub async fn await_topic(&self, topic: &str) -> ConduitValue {
        let mut rx = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = state.latest.remove(topic) {
                return existing;
            }
            state
                .channels
                .entry(topic.to_owned())
                .or_insert_with(|| broadcast::channel(64).0)
                .subscribe()
        };
        rx.recv().await.unwrap_or(ConduitValue::Unit)
    }
}

/// Pending human-in-the-loop interrupts awaiting external `resume_interrupt` resolution.
#[derive(Default)]
pub struct InterruptRegistry {
    waiters: StdMutex<HashMap<String, (String, oneshot::Sender<String>)>>,
}

impl InterruptRegistry {
    pub fn register(&self, id: &str, prompt: &str) -> oneshot::Receiver<String> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_owned(), (prompt.to_owned(), tx));
        rx
    }

    pub fn resume(&self, id: &str, response: String) -> Result<(), ConduitError> {
        let entry = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        match entry {
            Some((_, tx)) => {
                let _ = tx.send(response);
                Ok(())
            }
            None => Err(ConduitError::UnknownInterrupt { id: id.to_owned() }),
        }
    }

    pub fn snapshot_pending(&self) -> Vec<crate::snapshot::PendingInterrupt> {
        let guard = self
            .waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut list: Vec<_> = guard
            .iter()
            .map(|(id, (prompt, _))| crate::snapshot::PendingInterrupt {
                id: id.clone(),
                prompt: prompt.clone(),
            })
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }
}
