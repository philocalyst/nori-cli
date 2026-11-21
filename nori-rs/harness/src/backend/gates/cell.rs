//! Where the compiled gates live: one cell per delegation tree.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use tokio::sync::watch;

use super::Gates;
use super::stop::ContinueCounts;

/// The longest a check waits for the first load of the session's modules (it normally takes well
/// under a second); after that the check goes on without gates rather than hold a tool call.
const FIRST_LOAD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
enum State {
    /// Nothing will load gates (a session built without its modules, as in some tests).
    Unloaded,
    /// The session's modules are being evaluated; a check waits for them.
    Loading,
    Ready(Arc<Gates>),
}

/// The session's compiled gates, replaced whenever its modules are loaded again, and what the
/// stop gates have spent. Read by clone of an `Arc`: no lock is held across an await.
#[derive(Debug)]
pub(crate) struct GateCell {
    state: watch::Sender<State>,
    empty: Arc<Gates>,
    continues: Mutex<ContinueCounts>,
}

impl Default for GateCell {
    fn default() -> Self {
        Self {
            state: watch::Sender::new(State::Unloaded),
            empty: Arc::default(),
            continues: Mutex::default(),
        }
    }
}

impl GateCell {
    /// The session's modules are about to load: until [`GateCell::publish`], a check waits for
    /// them, so the first request of a session is never decided as if there were no gates. Does
    /// nothing once gates were published.
    pub(crate) fn expect_load(&self) {
        self.state.send_if_modified(|state| match state {
            State::Unloaded => {
                *state = State::Loading;
                true
            }
            State::Loading | State::Ready(_) => false,
        });
    }

    /// Replace the gates in force.
    pub(crate) fn publish(&self, gates: Arc<Gates>) {
        self.state.send_replace(State::Ready(gates));
    }

    /// The gates in force. Waits (at most a few seconds) only while the first load is running.
    pub(crate) async fn current(&self) -> Arc<Gates> {
        match &*self.state.borrow() {
            State::Ready(gates) => return Arc::clone(gates),
            State::Unloaded => return Arc::clone(&self.empty),
            State::Loading => {}
        }
        let mut changes = self.state.subscribe();
        let loaded = tokio::time::timeout(
            FIRST_LOAD,
            changes.wait_for(|state| !matches!(state, State::Loading)),
        )
        .await;
        match loaded {
            Ok(Ok(state)) => match &*state {
                State::Ready(gates) => Arc::clone(gates),
                State::Unloaded | State::Loading => Arc::clone(&self.empty),
            },
            Ok(Err(_)) | Err(_) => Arc::clone(&self.empty),
        }
    }

    /// Runs `check` over what the stop gates have spent in the current task.
    pub(crate) fn with_continues<T>(&self, check: impl FnOnce(&mut ContinueCounts) -> T) -> T {
        check(
            &mut self
                .continues
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
}
