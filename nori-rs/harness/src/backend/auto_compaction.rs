//! Automatic compaction, the backend's half: the turn ledger decides when (see
//! [`super::turn::pressure`]); this performs it and keeps the summary written ahead of need.
//!
//! Every path ends in [`AcpBackend::compaction_finished`], which tells the ledger how it ended:
//! a compacted session is measured again from the agent's next report, a prepared summary is
//! kept, and a failed automatic compaction stops the session compacting itself (said once).
//! The ledger holds the fence while it asks, so no prompt starts on the session being replaced.

use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::compaction::CompactionOutcome;
use super::compaction::compaction_prompt;
use super::session_runtime_driver::SessionRuntimeInput;
use super::turn::pressure::CompactHow;
use super::turn::pressure::CompactionEnd;
use super::*;
use crate::normalized::session_runtime::ContextUsage;
use crate::normalized::session_runtime::QueuedPromptKind;

/// A summary written ahead of need, for the session it describes.
struct Prepared {
    summary: String,
    session_id: acp::SessionId,
}

/// What automatic compaction keeps between turns.
#[derive(Default)]
pub(super) struct AutoCompaction {
    /// The summary turn that is running was asked for ahead of need: keep its summary.
    preparing: AtomicBool,
    prepared: StdMutex<Option<Prepared>>,
}

impl AutoCompaction {
    /// The running summary turn is a preparation, not a compaction.
    pub(super) fn preparing(&self) -> bool {
        self.preparing.load(Ordering::Acquire)
    }

    /// Keep `summary` of `session_id` for the swap the trigger will ask for.
    pub(super) fn store_prepared(&self, summary: String, session_id: acp::SessionId) {
        *self.prepared.lock().unwrap_or_else(PoisonError::into_inner) = Some(Prepared {
            summary,
            session_id,
        });
    }

    /// The summary prepared for `session_id`, if that is the one kept.
    fn take_prepared(&self, session_id: &acp::SessionId) -> Option<String> {
        self.prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
            .filter(|prepared| prepared.session_id == *session_id)
            .map(|prepared| prepared.summary)
    }
}

/// The parts of the session's policy the turn ledger applies.
pub(super) struct SessionPolicy {
    pub(super) compaction: nori_workflow::CompactionPolicy,
    pub(super) loops: nori_workflow::LoopPolicy,
}

impl AcpBackend {
    /// The session's policy once its workflow policy has loaded.
    pub(super) async fn session_policy(&self) -> SessionPolicy {
        let mut ready = self.workflow_ready.subscribe();
        while !*ready.borrow_and_update() {
            if ready.changed().await.is_err() {
                break;
            }
        }
        self.session_policy_now().await
    }

    /// The session's policy as loaded so far: the built-in one until the workflow policy has
    /// loaded. For the ledger's hot path, which never waits for Pkl.
    pub(super) async fn session_policy_now(&self) -> SessionPolicy {
        let workflow = self.workflow.lock().await;
        SessionPolicy {
            compaction: workflow.compaction.clone(),
            loops: workflow.loops,
        }
    }

    /// The ledger asks for a compaction before anything else starts; it holds the fence. The
    /// work runs off the runtime actor, and talks to the ledger through its queue.
    pub(super) fn compact_first(&self, usage: ContextUsage, how: CompactHow) {
        let backend = self.clone();
        tokio::spawn(async move {
            let percent = usage.used_tokens.saturating_mul(100) / usage.window_tokens.max(1);
            let session_id = backend.session_id.read().await.clone();
            let prepared = match how {
                CompactHow::Prepared { used_tokens } => backend
                    .auto_compaction
                    .take_prepared(&session_id)
                    .filter(|_| {
                        // What happened since the summary was written reaches the new session only
                        // as the checkpoint's verbatim recent context; more than that holds, and a
                        // summary written now loses less.
                        usage.used_tokens.saturating_sub(used_tokens)
                            <= crate::compact::RECENT_CONTEXT_TOKENS
                    }),
                CompactHow::Native | CompactHow::Summary => None,
            };
            if let Some(summary) = prepared {
                backend
                    .notice(&format!(
                        "Context {percent}% full; switching to the summary prepared earlier."
                    ))
                    .await;
                let outcome = if backend.install_summary(summary).await {
                    CompactionOutcome::Swapped
                } else {
                    CompactionOutcome::Failed
                };
                backend.compaction_finished(outcome).await;
                backend.release_fence().await;
                return;
            }
            backend
                .notice(&format!(
                    "Context {percent}% full; compacting before the next prompt."
                ))
                .await;
            let prompt = match how {
                CompactHow::Native => {
                    compaction_prompt(QueuedPromptKind::NativeCompact, &generate_id(), "/compact")
                }
                CompactHow::Prepared { .. } | CompactHow::Summary => {
                    backend.compact_nudged.store(false, Ordering::Release);
                    compaction_prompt(
                        QueuedPromptKind::Compact,
                        &generate_id(),
                        crate::compact::SUMMARIZATION_PROMPT,
                    )
                }
            };
            // First in line, then the fence opens: the compaction runs before what waited.
            backend
                .to_ledger(session_reducer::InboundEvent::PromptSteer(prompt))
                .await;
            backend.release_fence().await;
        });
    }

    /// The context is close to the trigger and the session is at rest: write the summary now,
    /// while the provider's cache is warm, and keep it.
    pub(super) fn prepare_summary(&self, usage: ContextUsage) {
        let backend = self.clone();
        tokio::spawn(async move {
            let percent = usage.used_tokens.saturating_mul(100) / usage.window_tokens.max(1);
            backend
                .auto_compaction
                .preparing
                .store(true, Ordering::Release);
            backend.compact_nudged.store(false, Ordering::Release);
            backend
                .notice(&format!(
                    "Context {percent}% full; preparing a summary while the cache is warm, for when it fills."
                ))
                .await;
            backend
                .to_ledger(session_reducer::InboundEvent::PromptSubmit(
                    compaction_prompt(
                        QueuedPromptKind::Compact,
                        &generate_id(),
                        crate::compact::SUMMARIZATION_PROMPT,
                    ),
                ))
                .await;
        });
    }

    /// A compaction of any origin ended: tell the ledger, before it releases the fence.
    pub(super) async fn compaction_finished(&self, outcome: CompactionOutcome) {
        let end = match outcome {
            CompactionOutcome::Swapped => CompactionEnd::Compacted,
            CompactionOutcome::Prepared => CompactionEnd::Prepared,
            CompactionOutcome::Failed | CompactionOutcome::Remind => CompactionEnd::Failed,
        };
        self.auto_compaction
            .preparing
            .store(false, Ordering::Release);
        if end == CompactionEnd::Compacted {
            // A summary kept for the session that was just replaced describes nothing now.
            let session_id = self.session_id.read().await.clone();
            self.auto_compaction.take_prepared(&session_id);
        }
        let stopped = self.session_driver.lock().await.compaction_ended(end);
        if stopped {
            self.notice(
                "Automatic compaction is off for this session after that failure; /compact when you want it.",
            )
            .await;
        }
    }

    async fn to_ledger(&self, event: session_reducer::InboundEvent) {
        let _ = self
            .session_event_tx
            .send(SessionRuntimeInput::Reducer(event))
            .await;
    }

    async fn release_fence(&self) {
        self.to_ledger(session_reducer::InboundEvent::ReleaseFence)
            .await;
    }
}
