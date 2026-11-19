//! What happens when a prompt arrives, and how the running turn is cancelled.
//!
//! Every agent answers the same three questions the same way, through one [`TurnGate`] per
//! session, so no agent keeps its own turn bookkeeping:
//!
//! - [`TurnPolicy::Exclusive`]: a prompt while a turn runs is refused with `Busy`.
//! - [`TurnPolicy::Steerable`]: the engine folds new input into the running turn (Codex's
//!   start-or-steer). The prompt gets a [`Turn`] of kind [`TurnKind::Steered`] that shares the
//!   running turn's cancellation, and the session is idle once every turn has ended.
//! - [`TurnPolicy::Replace`]: a newer prompt supersedes the running one (Claude). The running turn
//!   is cancelled, the gate waits a bounded time for it to end, then the new turn runs
//!   ([`TurnKind::Replaced`]).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::time::Duration;

use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;

/// How long a replacing prompt waits for the turn it cancelled to end before it runs anyway.
const REPLACE_TIMEOUT: Duration = Duration::from_secs(5);

/// What a prompt that arrives while a turn is running does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TurnPolicy {
    /// Refuse it.
    #[default]
    Exclusive,
    /// Join the running turn.
    Steerable,
    /// Cancel the running turn and take its place.
    Replace,
}

/// How a turn began.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnKind {
    /// The session was idle.
    Started,
    /// It joined a turn that was already running.
    Steered,
    /// It superseded a turn that was running.
    Replaced,
}

#[derive(Debug)]
struct State {
    /// Turns that have begun and not yet ended.
    live: i64,
    /// The cancellation shared by the turns that are running now.
    token: CancellationToken,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    idle: Notify,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Completes when no turn is running.
    async fn idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.state().live == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// A session's turn lock. [`TurnGate::begin`] admits a prompt according to the policy; the
/// [`Turn`] it returns is the proof that the turn is running and ends it when dropped.
#[derive(Debug)]
pub struct TurnGate {
    policy: TurnPolicy,
    shared: Arc<Shared>,
}

impl Default for TurnGate {
    fn default() -> Self {
        Self::new(TurnPolicy::Exclusive)
    }
}

/// One running turn. Dropping it ends the turn.
#[derive(Debug)]
pub struct Turn {
    shared: Arc<Shared>,
    token: CancellationToken,
    kind: TurnKind,
}

impl TurnGate {
    pub fn new(policy: TurnPolicy) -> Self {
        Self {
            policy,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    live: 0,
                    token: CancellationToken::new(),
                }),
                idle: Notify::new(),
            }),
        }
    }

    pub fn policy(&self) -> TurnPolicy {
        self.policy
    }

    /// Admit a prompt: start a turn, join the running one, replace it, or say that the session is
    /// busy, as the policy decides.
    pub async fn begin(&self) -> Result<Turn, AgentError> {
        let mut kind = TurnKind::Started;
        match self.policy {
            TurnPolicy::Exclusive => {
                if self.shared.state().live > 0 {
                    return Err(AgentError::Busy(
                        "A prompt is already running in this session".to_owned(),
                    ));
                }
            }
            TurnPolicy::Steerable => {
                if self.shared.state().live > 0 {
                    kind = TurnKind::Steered;
                }
            }
            TurnPolicy::Replace => {
                if self.cancel() {
                    kind = TurnKind::Replaced;
                    if tokio::time::timeout(REPLACE_TIMEOUT, self.shared.idle())
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "the replaced turn did not end within {}s; starting the new one anyway",
                            REPLACE_TIMEOUT.as_secs()
                        );
                    }
                }
            }
        }
        let mut state = self.shared.state();
        // A steered turn shares the running turn's cancellation; any other turn gets its own.
        if kind != TurnKind::Steered || state.live == 0 {
            state.token = CancellationToken::new();
            if kind == TurnKind::Steered {
                kind = TurnKind::Started;
            }
        }
        state.live += 1;
        Ok(Turn {
            shared: Arc::clone(&self.shared),
            token: state.token.clone(),
            kind,
        })
    }

    /// Completes when no turn is running (at once if none is).
    pub async fn idle(&self) {
        self.shared.idle().await;
    }

    /// Whether a turn is running.
    pub fn is_running(&self) -> bool {
        self.shared.state().live > 0
    }

    /// Cancel the running turn (and every turn steered into it). Returns whether one was running;
    /// cancelling an idle session does nothing.
    pub fn cancel(&self) -> bool {
        let state = self.shared.state();
        let running = state.live > 0;
        if running {
            state.token.cancel();
        }
        running
    }
}

impl Turn {
    pub fn kind(&self) -> TurnKind {
        self.kind
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Completes when the turn is cancelled.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }

    /// A token that a spawned task can watch after the turn's own future moves on.
    pub fn token(&self) -> CancellationToken {
        self.token.clone()
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        let mut state = self.shared.state();
        state.live -= 1;
        if state.live == 0 {
            drop(state);
            self.shared.idle.notify_waiters();
        }
    }
}
