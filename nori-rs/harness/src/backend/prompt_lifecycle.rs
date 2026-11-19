//! Prompt outcomes and cancellation belong to a wire request, never its successor.

use super::AcpBackend;
use super::session_reducer::CancelTarget;
use super::session_reducer::InboundEvent;
use crate::normalized::TurnFailure;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

const CANCEL_WARNING: std::time::Duration = std::time::Duration::from_secs(3);
pub(super) const CANCEL_FORCE: std::time::Duration = std::time::Duration::from_secs(10);

/// How a local request ended, as the transport saw it.
#[derive(Debug)]
pub enum PromptResult {
    Response(acp::StopReason),
    Failed(TurnFailure),
}

/// Cancellation ownership stays attached even when a shared slot is replaced.
pub(super) struct PromptTask {
    pub(super) request_id: acp::RequestId,
    pub(super) handle: AbortHandle,
}

impl PromptTask {
    pub(super) fn abort(self) {
        self.handle.abort();
    }
    fn abort_for(&self, request_id: &acp::RequestId) -> Option<AbortHandle> {
        (&self.request_id == request_id).then(|| self.handle.clone())
    }
}

impl AcpBackend {
    /// Send `session/cancel` for a turn the ledger cancelled. The first cancel of a local request
    /// also starts a watchdog: if the agent never answers, Nori aborts the request and ends it
    /// with an explicit failure. An agent turn has no request to abort; the ledger runs its own
    /// deadline.
    pub(super) async fn send_cancel(&self, target: CancelTarget, arm_watchdog: bool) {
        // A retry waiting out its backoff has no request in flight to cancel; stop it here.
        self.recovery.cancel();
        let session_id = self.session_id.read().await.clone();
        tracing::debug!(target: "acp_event_flow", %session_id, ?target, "Sending ACP session/cancel notification");
        // Capture this task now. A delayed watchdog must never take the shared
        // slot after a newer prompt has installed its own abort handle.
        let watchdog_target = match target {
            CancelTarget::Request(request_id) if arm_watchdog => Some((
                self.prompt_task_abort
                    .lock()
                    .await
                    .as_ref()
                    .and_then(|task| task.abort_for(&request_id)),
                request_id,
            )),
            CancelTarget::Request(_) | CancelTarget::AgentTurn(_) => None,
        };
        if let Err(error) = self.connection.cancel(&session_id).await {
            tracing::warn!(%error, "Failed to cancel ACP session");
        }
        let Some((abort, request_id)) = watchdog_target else {
            return;
        };
        let event_tx = self.backend_event_tx.clone();
        let result_tx = self.prompt_result_tx.clone();
        let owner = request_id.clone();
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(CANCEL_WARNING).await;
            tracing::warn!(%request_id, "Agent has not responded to cancel after {}s", CANCEL_WARNING.as_secs());
            let _ = event_tx
                .send(super::BackendEvent::Public(
                    nori_protocol::SessionEvent::Nori(nori_protocol::NoriEvent::Notice(
                        nori_protocol::Notice {
                            message: format!(
                                "Agent is slow to cancel. Will force-cancel in {}s…",
                                (CANCEL_FORCE - CANCEL_WARNING).as_secs()
                            ),
                        },
                    )),
                ))
                .await;
            tokio::time::sleep(CANCEL_FORCE - CANCEL_WARNING).await;
            force_cancel(request_id, abort, &result_tx).await;
        });
        *self.cancel_timeout_abort.lock().await = Some(PromptTask {
            request_id: owner,
            handle: watchdog.abort_handle(),
        });
    }
}

/// The agent ignored `session/cancel`: stop waiting on its response and end the request as an
/// explicit failure, not a cancellation the agent never confirmed.
async fn force_cancel(
    request_id: acp::RequestId,
    abort: Option<AbortHandle>,
    result_tx: &mpsc::Sender<InboundEvent>,
) {
    if let Some(abort) = abort {
        abort.abort();
    }
    let _ = result_tx
        .send(InboundEvent::PromptResultFor {
            request_id,
            result: PromptResult::Failed(TurnFailure::CancelIgnored),
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::super::session_runtime_driver::SessionDriver;
    use super::*;
    use crate::normalized::session_runtime::QueuedPrompt;
    use crate::normalized::session_runtime::QueuedPromptKind;
    use pretty_assertions::assert_eq;

    fn start(driver: &mut SessionDriver, id: &str) {
        driver.apply(InboundEvent::PromptSubmit(QueuedPrompt {
            event_id: id.into(),
            kind: QueuedPromptKind::User,
            text: id.into(),
            prompt_meta: None,
            original_content: vec![],
            content: vec![],
            display_text: Some(id.into()),
        }));
        driver.apply(InboundEvent::PromptStarted {
            request_id: acp::RequestId::Str(id.into()),
        });
    }

    #[tokio::test]
    async fn late_timeout_and_completion_cannot_abort_or_complete_a_successor() {
        let mut driver = SessionDriver::new();
        start(&mut driver, "old");
        let old = tokio::spawn(std::future::pending::<()>());
        let old_task = PromptTask {
            request_id: acp::RequestId::Str("old".into()),
            handle: old.abort_handle(),
        };
        let old_abort = old_task.abort_for(&acp::RequestId::Str("old".into()));
        driver.apply(InboundEvent::PromptResultFor {
            request_id: acp::RequestId::Str("old".into()),
            result: PromptResult::Response(acp::StopReason::Cancelled),
        });
        start(&mut driver, "successor");
        let successor = tokio::spawn(std::future::pending::<()>());
        let successor_task = PromptTask {
            request_id: acp::RequestId::Str("successor".into()),
            handle: successor.abort_handle(),
        };
        assert!(
            successor_task
                .abort_for(&acp::RequestId::Str("old".into()))
                .is_none()
        );
        let (tx, mut rx) = mpsc::channel(1);
        force_cancel(acp::RequestId::Str("old".into()), old_abort, &tx).await;
        let stale_timeout = driver.apply(
            rx.recv()
                .await
                .unwrap_or_else(|| panic!("watchdog result channel closed")),
        );
        let stale_completion = driver.apply(InboundEvent::PromptResultFor {
            request_id: acp::RequestId::Str("old".into()),
            result: PromptResult::Response(acp::StopReason::EndTurn),
        });
        let Err(cancelled) = old.await else {
            panic!("old task unexpectedly completed")
        };
        assert!(cancelled.is_cancelled());
        assert!(!successor.is_finished());
        assert_eq!(driver.active_request_id(), Some("successor".into()));
        for result in [stale_timeout, stale_completion] {
            assert!(result.events.is_empty());
            assert!(result.side_effects.is_empty());
            assert!(result.completed_turn.is_none());
        }
        let completed = driver.apply(InboundEvent::PromptResultFor {
            request_id: acp::RequestId::Str("successor".into()),
            result: PromptResult::Response(acp::StopReason::EndTurn),
        });
        assert!(completed.completed_turn.is_some());
        assert_eq!(driver.active_request_id(), None);
        successor.abort();
    }
}
