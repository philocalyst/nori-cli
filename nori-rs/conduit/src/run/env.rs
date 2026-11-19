//! Shared state of one run: the host, the journal, actors, worlds and the latest bindings.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use nori_workflow::RecoveryOverride;
use tokio::sync::Mutex;
use tokio::sync::watch;

use crate::error::ConduitError;
use crate::host::ActorRef;
use crate::host::InterruptRegistry;
use crate::host::ProgramHost;
use crate::host::Receipt;
use crate::host::SignalHub;
use crate::host::WorldScope;
use crate::op::CompiledProgram;
use crate::policy::BudgetFrame;
use crate::schemas::SandboxPolicy;
use crate::snapshot::ActorState;
use crate::snapshot::PendingInterrupt;
use crate::snapshot::RunSnapshot;
use crate::snapshot::RunStatus;
use crate::snapshot::WorldState;
use crate::tools::ToolRuntime;
use crate::transport::Transport;
use crate::value::ConduitValue;
use crate::wire::Envelope;
use crate::wire::PROGRAM_ACTOR;
use crate::wire::WireOp;

/// What a run keeps besides its bindings: spend, pins, checkpoints and open interrupts.
#[derive(Default)]
pub(crate) struct RunState {
    pub tokens_spent: i64,
    pub budget_tokens: Option<i64>,
    pub pins: Vec<String>,
    pub checkpoints: Vec<String>,
    pub tool_calls: i64,
    pub pending_interrupts: Vec<PendingInterrupt>,
}

/// An actor the run has started.
pub(crate) struct ActorRuntime {
    pub actor: ActorRef,
    pub turns: i64,
    /// Fallback routes already used, in order; the next one is `chain[fallbacks.len()]`.
    pub fallbacks: Vec<String>,
}

/// A message handed to an actor whose reply has not been collected yet.
#[derive(Clone)]
pub(crate) struct PendingTurn {
    pub alias: String,
    pub receipt: Receipt,
    pub message: String,
    pub delivery: crate::host::Delivery,
    /// Recovery in force when the message was sent, so a later `Await` recovers the same way.
    pub recovery: RecoveryOverride,
}

/// What differs between concurrent flows of the program; cloned for every branch.
#[derive(Clone)]
pub(crate) struct Scope {
    pub bindings: BTreeMap<String, ConduitValue>,
    pub receipts: BTreeMap<String, PendingTurn>,
    pub sandbox: Option<SandboxPolicy>,
    pub budget: Option<Arc<BudgetFrame>>,
    pub world: Option<String>,
    /// Recovery overrides of the enclosing steps, outermost first.
    pub recovery: RecoveryOverride,
    pub last: ConduitValue,
    /// Names the iteration of enclosing fan-outs (`[item=a]`), so concurrent runs of the same
    /// step are told apart in the journal.
    pub tag: String,
}

pub(crate) struct Env {
    pub run: String,
    pub program: CompiledProgram,
    pub source: Option<String>,
    pub base_dir: PathBuf,
    pub run_dir: Option<PathBuf>,
    pub host: Arc<dyn ProgramHost>,
    pub tools: Arc<dyn ToolRuntime>,
    pub transport: Arc<dyn Transport>,
    pub signals: Arc<SignalHub>,
    pub interrupts: Arc<InterruptRegistry>,
    pub state: StdMutex<RunState>,
    pub cancel: watch::Receiver<bool>,
    pub seq: AtomicI64,
    pub actors: Mutex<BTreeMap<String, ActorRuntime>>,
    pub worlds: Mutex<BTreeMap<String, WorldScope>>,
    pub bindings: StdMutex<BTreeMap<String, String>>,
}

impl Env {
    pub(crate) fn lock_state(&self) -> std::sync::MutexGuard<'_, RunState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Journal one fact. A failure to journal fails the run: nothing happens unrecorded.
    pub(crate) async fn emit(
        &self,
        actor: &str,
        turn: i64,
        op: WireOp,
        payload: serde_json::Value,
    ) -> Result<(), ConduitError> {
        let seq = self.seq.fetch_add(1, Ordering::AcqRel) + 1;
        let envelope = Envelope::new(&self.run, actor, turn, seq, op, payload);
        self.transport.send(&envelope).await
    }

    pub(crate) async fn emit_run(
        &self,
        op: WireOp,
        payload: serde_json::Value,
    ) -> Result<(), ConduitError> {
        self.emit(PROGRAM_ACTOR, 0, op, payload).await
    }

    /// Record the latest value of a binding for the snapshot.
    pub(crate) fn note_binding(&self, name: &str, value: &ConduitValue) {
        self.bindings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name.to_owned(), value.display_compact());
    }

    pub(crate) fn charge_tokens(
        &self,
        budget: Option<&Arc<BudgetFrame>>,
        tokens: i64,
    ) -> Result<(), ConduitError> {
        let delta = tokens.max(0);
        if let Some(frame) = budget {
            frame.charge(delta)?;
        }
        let mut state = self.lock_state();
        if let Some(limit) = state.budget_tokens {
            let next = state.tokens_spent.saturating_add(delta);
            if next > limit {
                return Err(ConduitError::BudgetExceeded {
                    limit,
                    spent: state.tokens_spent,
                    requested: delta,
                });
            }
        }
        state.tokens_spent = state.tokens_spent.saturating_add(delta);
        Ok(())
    }

    /// Rewrite `state.pkl` with the run as it stands.
    pub(crate) async fn write_snapshot(
        &self,
        status: RunStatus,
        result: Option<&ConduitValue>,
        error: Option<&ConduitError>,
    ) -> Result<(), ConduitError> {
        let Some(dir) = &self.run_dir else {
            return Ok(());
        };
        let actors = self
            .actors
            .lock()
            .await
            .iter()
            .map(|(alias, runtime)| {
                (
                    alias.clone(),
                    ActorState {
                        agent: runtime.actor.agent.clone(),
                        model: runtime.actor.model.clone(),
                        session: runtime.actor.session.clone(),
                        turns: runtime.turns,
                        fallbacks: runtime.fallbacks.clone(),
                    },
                )
            })
            .collect();
        let worlds = self
            .worlds
            .lock()
            .await
            .iter()
            .map(|(name, world)| {
                (
                    name.clone(),
                    WorldState {
                        isolation: format!("{:?}", world.isolation).to_lowercase(),
                        root: world.root.display().to_string(),
                        home: world.home.as_ref().map(|home| home.display().to_string()),
                    },
                )
            })
            .collect();
        let state = self.lock_state();
        let snapshot = RunSnapshot {
            run: self.run.clone(),
            program: self.program.name.clone(),
            source: self.source.clone(),
            status,
            seq: self.seq.load(Ordering::Acquire),
            tokens: state.tokens_spent,
            tool_calls: state.tool_calls,
            pins: state.pins.clone(),
            checkpoints: state.checkpoints.clone(),
            pending_interrupts: state.pending_interrupts.clone(),
            actors,
            worlds,
            bindings: self
                .bindings
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
            result: result.map(ConduitValue::display_compact),
            error: error.map(ToString::to_string),
        };
        drop(state);
        snapshot.write(&dir.join("state.pkl"))
    }
}
