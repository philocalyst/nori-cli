//! What a program's actors really spent, from the usage their agents report.
//!
//! Every usage sample the process records carries the actor that spent it. The meter keeps a
//! running total per actor; a turn is charged what its actor's total grew by between the send
//! and the reply. A turn whose agent reported nothing has no real figure, and the host says so
//! (the caller then estimates and labels the number). Input and output tokens count; cache
//! reads are traffic, not spend.

use std::collections::HashMap;
use std::sync::Arc;

use nori_conduit::Receipt;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use super::host::HarnessProgramHost;

#[derive(Default)]
struct Totals {
    /// Tokens reported per actor (its id as the user's view shows it).
    spent: HashMap<String, i64>,
    /// How much an actor had spent when each turn was sent, by receipt id.
    baseline: HashMap<String, (String, i64)>,
}

pub(super) struct UsageMeter {
    totals: Arc<Mutex<Totals>>,
    listener: JoinHandle<()>,
}

impl UsageMeter {
    /// Start listening to the process's usage samples. Needs a Tokio runtime.
    pub(super) fn start() -> Self {
        let totals = Arc::new(Mutex::new(Totals::default()));
        let mut samples = crate::usage::subscribe_samples();
        let recorded = Arc::clone(&totals);
        let listener = tokio::spawn(async move {
            loop {
                match samples.recv().await {
                    Ok(sample) => {
                        if let Some(actor) = sample.actor {
                            let spent = sample
                                .tokens
                                .input
                                .saturating_add(sample.tokens.output)
                                .saturating_add(sample.tokens.cache_write);
                            let mut totals = recorded.lock().await;
                            let total = totals.spent.entry(actor).or_insert(0);
                            *total = total.saturating_add(spent);
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        Self { totals, listener }
    }
}

impl Drop for UsageMeter {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

impl HarnessProgramHost {
    /// Remember what `actor` had spent when the turn `receipt` was sent.
    pub(super) async fn mark_usage(&self, actor: &str, receipt: &str) {
        let mut totals = self.usage.totals.lock().await;
        let now = totals.spent.get(actor).copied().unwrap_or(0);
        totals
            .baseline
            .insert(receipt.to_owned(), (actor.to_owned(), now));
    }

    /// What the turn's actor reported spending since the turn was sent; `None` when it reported
    /// nothing at all.
    pub(super) async fn reported_since(&self, receipt: &Receipt) -> Option<i64> {
        let mut totals = self.usage.totals.lock().await;
        let (actor, before) = totals.baseline.remove(&receipt.id)?;
        let now = totals.spent.get(&actor).copied().unwrap_or(0);
        (now > before).then_some(now - before)
    }
}
