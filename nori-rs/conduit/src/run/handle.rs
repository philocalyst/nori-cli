//! The live capability over a running program: what a supervisor, a daemon or a UI holds.
//!
//! A [`RunHandle`] starts a run in the background and offers what the run offers every other
//! world: watch its envelopes as they happen, read its latest snapshot, send it signals and
//! interrupt answers, cancel it, and wait for its outcome. It is built entirely from the
//! run's [`Transport`]: inbound calls become inbound envelopes, and the observer is a mirror.

use std::path::PathBuf;
use std::sync::Arc;

use async_stream::stream;
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::RunConfig;
use super::RunOutcome;
use super::new_run_id;
use super::run_program;
use crate::error::ConduitError;
use crate::op::CompiledProgram;
use crate::snapshot::RunSnapshot;
use crate::transport::Transport;
use crate::wire::Envelope;
use crate::wire::PROGRAM_ACTOR;
use crate::wire::WireOp;

/// The run's transport as the handle shapes it: sends go to the real transport and to the
/// observers; receives come from the handle first and then from the real transport.
struct HandleTransport {
    primary: Arc<dyn Transport>,
    observers: broadcast::Sender<Envelope>,
    from_handle: Mutex<mpsc::UnboundedReceiver<Envelope>>,
}

#[async_trait]
impl Transport for HandleTransport {
    async fn send(&self, envelope: &Envelope) -> Result<(), ConduitError> {
        self.primary.send(envelope).await?;
        let _ = self.observers.send(envelope.clone());
        Ok(())
    }

    async fn recv(&self) -> Result<Option<Envelope>, ConduitError> {
        let mut from_handle = self.from_handle.lock().await;
        tokio::select! {
            envelope = from_handle.recv() => Ok(envelope),
            envelope = self.primary.recv() => envelope,
        }
    }
}

pub struct RunHandle {
    run: String,
    run_dir: Option<PathBuf>,
    observers: broadcast::Sender<Envelope>,
    inbound: mpsc::UnboundedSender<Envelope>,
    outcome: JoinHandle<Result<RunOutcome, ConduitError>>,
}

impl RunHandle {
    /// Start `program` in the background. Its journal goes to `config.transport` as usual.
    pub fn start(program: CompiledProgram, mut config: RunConfig) -> Self {
        let run = config.run_id.clone().unwrap_or_else(new_run_id);
        config.run_id = Some(run.clone());
        let (observers, _) = broadcast::channel(1024);
        let (inbound, from_handle) = mpsc::unbounded_channel();
        let run_dir = config.run_dir.clone();
        config.transport = Arc::new(HandleTransport {
            primary: Arc::clone(&config.transport),
            observers: observers.clone(),
            from_handle: Mutex::new(from_handle),
        });
        Self {
            run,
            run_dir,
            observers,
            inbound,
            outcome: tokio::spawn(run_program(program, config)),
        }
    }

    pub fn run_id(&self) -> &str {
        &self.run
    }

    /// Every envelope the run writes from now on, in order.
    pub fn subscribe(&self) -> BoxStream<'static, Envelope> {
        let mut events = self.observers.subscribe();
        Box::pin(stream! {
            loop {
                match events.recv().await {
                    Ok(envelope) => yield envelope,
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    fn send(&self, op: WireOp, payload: serde_json::Value) -> Result<(), ConduitError> {
        self.inbound
            .send(Envelope::new(&self.run, PROGRAM_ACTOR, 0, 0, op, payload))
            .map_err(|_| ConduitError::Runtime {
                message: format!("run {} has ended", self.run),
            })
    }

    /// Wake `AwaitSignal` steps waiting for `topic`.
    pub fn signal(&self, topic: &str, payload: serde_json::Value) -> Result<(), ConduitError> {
        self.send(WireOp::Signal, json!({"topic": topic, "payload": payload}))
    }

    /// Answer the interrupt `id` (see `pending_interrupts` of the snapshot).
    pub fn resume_interrupt(&self, id: &str, response: &str) -> Result<(), ConduitError> {
        self.send(
            WireOp::InterruptResumed,
            json!({"interrupt": id, "response": response}),
        )
    }

    pub fn cancel(&self, reason: &str) -> Result<(), ConduitError> {
        self.send(WireOp::RunCancel, json!({"reason": reason}))
    }

    /// The run's latest snapshot, read back from its `state.pkl`. A run with no directory has
    /// none to read.
    pub async fn snapshot(&self) -> Result<RunSnapshot, ConduitError> {
        let Some(dir) = &self.run_dir else {
            return Err(ConduitError::Unsupported {
                what: "a run snapshot".to_owned(),
                reason: "the run was started without a directory, so it keeps no state.pkl"
                    .to_owned(),
            });
        };
        RunSnapshot::load(&dir.join("state.pkl"))
    }

    /// Wait for the run to end.
    pub async fn wait(self) -> Result<RunOutcome, ConduitError> {
        self.outcome.await.map_err(|error| ConduitError::Runtime {
            message: format!("the run task stopped: {error}"),
        })?
    }
}
