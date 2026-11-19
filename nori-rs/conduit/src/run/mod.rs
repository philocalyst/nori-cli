//! Execute a [`CompiledProgram`] against a host, journaling every step.
//!
//! [`run_program`] is the single entry point. It checks that declared tools can run, opens the
//! journal, starts a pump that feeds the other world's envelopes (signals, interrupt answers,
//! cancellation) into the run, executes the op graph and finishes with `run.finished` or
//! `run.failed`, rewriting `state.pkl` along the way.

mod actors;
mod control;
mod env;
mod eval;
mod handle;

pub use handle::RunHandle;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::watch;

use self::env::Env;
use self::env::RunState;
use self::env::Scope;
use crate::error::ConduitError;
use crate::host::InterruptRegistry;
use crate::host::ProgramHost;
use crate::host::SignalHub;
use crate::op::CompiledProgram;
use crate::op::Node;
use crate::op::Op;
use crate::op::ToolRef;
use crate::policy::BudgetFrame;
use crate::schemas::SandboxPolicy;
use crate::schemas::ToolSource;
use crate::snapshot::RunStatus;
use crate::tools::ToolRuntime;
use crate::tools::UnavailableToolRuntime;
use crate::transport::Transport;
use crate::value::ConduitValue;
use crate::wire::WireOp;

/// Everything a run needs besides the program.
pub struct RunConfig {
    /// The run's id, when the caller already named the directory after it.
    pub run_id: Option<String>,
    pub host: Arc<dyn ProgramHost>,
    /// Where the run's envelopes go (and where inbound ones come from).
    pub transport: Arc<dyn Transport>,
    /// Runs the program's `wasm` tools.
    pub tools: Arc<dyn ToolRuntime>,
    /// The run's directory: `state.pkl` and worlds live here. `None` keeps the run in memory.
    pub run_dir: Option<PathBuf>,
    /// The program file, recorded in the journal and snapshot, and the base of wasm paths.
    pub source: Option<PathBuf>,
    /// The sandbox already in force around the program.
    pub sandbox: Option<SandboxPolicy>,
    pub signals: Arc<SignalHub>,
    pub interrupts: Arc<InterruptRegistry>,
}

impl RunConfig {
    pub fn new(host: Arc<dyn ProgramHost>, transport: Arc<dyn Transport>) -> Self {
        Self {
            run_id: None,
            host,
            transport,
            tools: Arc::new(UnavailableToolRuntime),
            run_dir: None,
            source: None,
            sandbox: None,
            signals: Arc::new(SignalHub::default()),
            interrupts: Arc::new(InterruptRegistry::default()),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunOutcome {
    pub run: String,
    pub value: ConduitValue,
    pub tokens: i64,
    /// The latest value of every binding, as text.
    pub bindings: BTreeMap<String, String>,
}

static RUN_COUNTER: AtomicI64 = AtomicI64::new(0);

/// A run id unique to this process and moment: `run-<unix ms in hex>-<counter>`.
pub fn new_run_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis());
    format!(
        "run-{millis:x}-{:03x}",
        RUN_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn declared_tools<'a>(node: &'a Node, out: &mut Vec<&'a crate::schemas::ToolSpec>) {
    if let Op::Call {
        tool: ToolRef::Declared { tool },
        ..
    } = &node.op
        && matches!(tool.source, ToolSource::Wasm { .. })
    {
        out.push(tool);
    }
    for child in node.children() {
        declared_tools(child, out);
    }
}

/// Run `program` to completion.
pub async fn run_program(
    program: CompiledProgram,
    config: RunConfig,
) -> Result<RunOutcome, ConduitError> {
    let mut wasm = Vec::new();
    declared_tools(&program.root, &mut wasm);
    for tool in wasm {
        config.tools.check(tool)?;
    }
    let run = config.run_id.unwrap_or_else(new_run_id);
    let base_dir = config
        .source
        .as_deref()
        .and_then(std::path::Path::parent)
        .map_or_else(|| config.host.workspace_root().to_path_buf(), PathBuf::from);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let env = Arc::new(Env {
        run: run.clone(),
        source: config
            .source
            .as_ref()
            .map(|path| path.display().to_string()),
        base_dir,
        run_dir: config.run_dir,
        host: config.host,
        tools: config.tools,
        transport: Arc::clone(&config.transport),
        signals: Arc::clone(&config.signals),
        interrupts: Arc::clone(&config.interrupts),
        state: StdMutex::new(RunState {
            budget_tokens: program.budget_tokens,
            ..RunState::default()
        }),
        cancel: cancel_rx,
        seq: AtomicI64::new(0),
        actors: Mutex::new(BTreeMap::new()),
        worlds: Mutex::new(BTreeMap::new()),
        bindings: StdMutex::new(BTreeMap::new()),
        program,
    });
    let pump = tokio::spawn(pump_inbound(
        Arc::clone(&config.transport),
        Arc::clone(&config.signals),
        Arc::clone(&config.interrupts),
        cancel_tx,
    ));
    let outcome = execute(&env, config.sandbox).await;
    pump.abort();
    // Worlds an actor opened implicitly stay open until the run ends.
    let still_open: Vec<_> = env.worlds.lock().await.values().cloned().collect();
    for world in still_open {
        control::close_world(&env, &world).await?;
    }
    let (status, result, error) = match &outcome {
        Ok(value) => (RunStatus::Finished, Some(value), None),
        Err(ConduitError::Cancelled) => (RunStatus::Cancelled, None, outcome.as_ref().err()),
        Err(error) => (RunStatus::Failed, None, Some(error)),
    };
    let tokens = env.lock_state().tokens_spent;
    match &outcome {
        Ok(value) => {
            env.emit_run(
                WireOp::RunFinished,
                json!({"value": value.to_plain_json(), "tokens": tokens}),
            )
            .await?;
        }
        Err(error) => {
            env.emit_run(WireOp::RunFailed, json!({"error": error.to_string()}))
                .await?;
        }
    }
    env.write_snapshot(status, result, error).await?;
    let bindings = env
        .bindings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Ok(RunOutcome {
        run,
        value: outcome?,
        tokens,
        bindings,
    })
}

async fn execute(
    env: &Arc<Env>,
    sandbox: Option<SandboxPolicy>,
) -> Result<ConduitValue, ConduitError> {
    env.emit_run(
        WireOp::RunStarted,
        json!({
            "program": env.program.name, "source": env.source,
            "ops": serde_json::to_value(&env.program).map_err(|error| ConduitError::Wire {
                message: format!("cannot encode the op graph: {error}"),
            })?,
        }),
    )
    .await?;
    env.write_snapshot(RunStatus::Running, None, None).await?;
    let mut scope = Scope {
        bindings: BTreeMap::new(),
        receipts: BTreeMap::new(),
        sandbox,
        budget: env.program.budget_tokens.map(BudgetFrame::root),
        world: None,
        recovery: nori_workflow::RecoveryOverride::default(),
        last: ConduitValue::Unit,
        tag: String::new(),
    };
    let root = std::slice::from_ref(&env.program.root);
    let last = if env.program.world.is_some() {
        control::within(env, "program", root, &mut scope).await?
    } else {
        eval::eval_seq(env, root, &mut scope).await?
    };
    match &env.program.result {
        Some(name) => scope
            .bindings
            .get(name)
            .cloned()
            .ok_or_else(|| ConduitError::UnboundName {
                name: name.clone(),
                bound: scope
                    .bindings
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
        None => Ok(last),
    }
}

/// Feed the other world's envelopes into the run until the transport closes.
async fn pump_inbound(
    transport: Arc<dyn Transport>,
    signals: Arc<SignalHub>,
    interrupts: Arc<InterruptRegistry>,
    cancel: watch::Sender<bool>,
) {
    while let Ok(Some(envelope)) = transport.recv().await {
        let field = |name: &str| {
            envelope
                .payload
                .get(name)
                .and_then(serde_json::Value::as_str)
        };
        match envelope.op {
            WireOp::Signal => {
                if let Some(topic) = field("topic") {
                    let payload = envelope.payload.get("payload").cloned().unwrap_or_default();
                    signals.emit(topic, ConduitValue::from_json(payload));
                }
            }
            WireOp::InterruptResumed => {
                if let (Some(id), Some(response)) = (field("interrupt"), field("response")) {
                    let _ = interrupts.resume(id, response.to_owned());
                }
            }
            WireOp::RunCancel => {
                let _ = cancel.send(true);
            }
            WireOp::RunStarted
            | WireOp::RunFinished
            | WireOp::RunFailed
            | WireOp::StepStarted
            | WireOp::StepFinished
            | WireOp::StepFailed
            | WireOp::ActorSpawned
            | WireOp::MessageSent
            | WireOp::MessageReplied
            | WireOp::RecoveryRetry
            | WireOp::RouteChanged
            | WireOp::ToolCalled
            | WireOp::InterruptRequested
            | WireOp::WorldOpened
            | WireOp::WorldClosed
            | WireOp::Note => {}
        }
    }
}
