//! Session configuration changes, off the command loop.
//!
//! A `session/set_config_option` waits for the agent's answer, which a busy or wedged agent can
//! take seconds to give. Answered inside the harness command loop, every command behind it waited
//! too, a Cancel included. The lane takes the changes instead: the command loop hands each one
//! over and goes on, and the lane applies them to the agent one at a time, in the order they were
//! made. Coalescing is best-effort: only repeated changes already queued when the lane drains a
//! batch can be folded together; changes that arrive after a request starts wait behind it. In a
//! drained batch, a later change to the same option of the same backend replaces the earlier one:
//! picking three models in a row sends the last if all three were queued before the batch began,
//! and all three callers hear how it went.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::oneshot;

use crate::backend::AcpBackend;

type Answer = oneshot::Sender<anyhow::Result<Vec<nori_protocol::acp::v1::SessionConfigOption>>>;

struct Change {
    /// The backend that was current when the change was made: a provider handoff swaps it.
    backend: Arc<AcpBackend>,
    config_id: String,
    value: String,
    answers: Vec<Answer>,
}

/// Where the command loop hands session configuration changes.
#[derive(Clone)]
pub(super) struct ConfigLane {
    changes: mpsc::UnboundedSender<Change>,
}

impl ConfigLane {
    pub(super) fn start() -> Self {
        let (changes, mut queue) = mpsc::unbounded_channel::<Change>();
        tokio::spawn(async move {
            while let Some(first) = queue.recv().await {
                let mut batch = vec![first];
                while let Ok(next) = queue.try_recv() {
                    batch.push(next);
                }
                for change in coalesce(batch) {
                    let result = change
                        .backend
                        .set_config_option(change.config_id, change.value)
                        .await
                        .map(|()| change.backend.config_options());
                    for answer in change.answers {
                        let _ = answer.send(match &result {
                            Ok(options) => Ok(options.clone()),
                            Err(error) => Err(anyhow::anyhow!("{error:#}")),
                        });
                    }
                }
            }
        });
        Self { changes }
    }

    /// Queue a change. `answer` hears the options as they stand once it (or a change that
    /// replaced it) was applied, or why it was not.
    pub(super) fn submit(
        &self,
        backend: Arc<AcpBackend>,
        config_id: String,
        value: String,
        answer: Answer,
    ) {
        if let Err(mpsc::error::SendError(change)) = self.changes.send(Change {
            backend,
            config_id,
            value,
            answers: vec![answer],
        }) {
            for answer in change.answers {
                let _ = answer.send(Err(anyhow::anyhow!("the session has ended")));
            }
        }
    }
}

/// `batch` in order, with every change that a later change to the same option of the same
/// backend replaces folded into that later change.
fn coalesce(batch: Vec<Change>) -> Vec<Change> {
    let mut kept: Vec<Change> = Vec::with_capacity(batch.len());
    for change in batch {
        if let Some(position) = kept.iter().position(|earlier| {
            earlier.config_id == change.config_id && Arc::ptr_eq(&earlier.backend, &change.backend)
        }) {
            let earlier = kept.remove(position);
            let mut answers = earlier.answers;
            answers.extend(change.answers);
            kept.push(Change { answers, ..change });
        } else {
            kept.push(change);
        }
    }
    kept
}
