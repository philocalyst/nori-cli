//! The parent session's side: what its backend does for the orchestrator.

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use tokio::sync::mpsc;

use super::Signal;
use crate::backend::AcpBackend;
use crate::backend::BackendEvent;
use crate::backend::PendingApprovalRequest;
use crate::connection::DelegatedRequest;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

/// Results owed within this long of each other reach the agent in one turn.
const WAKE_BATCH: std::time::Duration = std::time::Duration::from_millis(600);

impl AcpBackend {
    /// Act on the orchestrator's signals until it stops sending them, at shutdown.
    pub(in crate::backend) fn serve_orchestrator(
        &self,
        mut signals: mpsc::UnboundedReceiver<Signal>,
    ) {
        self.serve_monitors();
        let backend = self.clone();
        tokio::spawn(async move {
            let wake_scheduled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            while let Some(signal) = signals.recv().await {
                match signal {
                    Signal::Approval(request) => backend.ask_for_child(request).await,
                    Signal::ResultsOwed => {
                        if !wake_scheduled.swap(true, std::sync::atomic::Ordering::AcqRel) {
                            let backend = backend.clone();
                            let wake_scheduled = std::sync::Arc::clone(&wake_scheduled);
                            tokio::spawn(async move {
                                tokio::time::sleep(WAKE_BATCH).await;
                                wake_scheduled.store(false, std::sync::atomic::Ordering::Release);
                                backend.deliver_task_results_if_idle().await;
                            });
                        }
                    }
                    Signal::MailboxWake => {
                        let backend = backend.clone();
                        tokio::spawn(async move {
                            backend.deliver_root_mailbox().await;
                        });
                    }
                    Signal::Withdraw(request_ids) => {
                        // Dropping a request's responder answers the child's relay as cancelled.
                        backend
                            .pending_approvals
                            .lock()
                            .await
                            .retain(|pending| !request_ids.contains(&pending.request.request_id));
                        for request_id in request_ids {
                            let _ = backend
                                .backend_event_tx
                                .send(BackendEvent::Public(SessionEvent::Nori(
                                    NoriEvent::RequestWithdrawn(request_id),
                                )))
                                .await;
                        }
                    }
                }
            }
        });
    }

    /// Ask the user, in this conversation, for a permission a child requested.
    /// Registers the pending request synchronously before non-blocking event dispatch
    /// so concurrent approval relays across multiple children never deadlock the signal loop.
    async fn ask_for_child(&self, request: Box<DelegatedRequest>) {
        let request = *request;
        let title = match &request.request {
            nori_protocol::acp::v1::AgentRequest::RequestPermissionRequest(permission) => {
                permission
                    .tool_call
                    .fields
                    .title
                    .clone()
                    .unwrap_or_else(|| "tool request".to_owned())
            }
            _ => "tool request".to_owned(),
        };
        let event = SessionEvent::Acp(AcpEvent::Request {
            request_id: request.request_id.clone(),
            request: request.request.clone(),
        });
        let call_id = request.request_id.to_string();
        self.pending_approvals
            .lock()
            .await
            .push(PendingApprovalRequest { request });
        match self.backend_event_tx.try_send(BackendEvent::Public(event)) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                let tx = self.backend_event_tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(event).await;
                });
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
        self.user_notifier
            .notify(&crate::UserNotification::AwaitingApproval {
                call_id,
                command: title,
                cwd: self.cwd.display().to_string(),
            });
    }

    /// Deliver the messages peers queued for the root session in one hidden turn, when it is idle.
    /// While a turn is active they stay queued (and arrive inline in the result of any task tool
    /// the root calls), so neither a cancelled turn nor a compaction can drop them.
    ///
    /// Taking the mail and submitting its turn happen under the rewind admission, and a rewind
    /// refuses to run while mail is queued, so mail from before a rewind can never be delivered
    /// into the conversation after it.
    pub(in crate::backend) async fn deliver_root_mailbox(&self) {
        let _admission = self.orchestrator.rewind_admission().await;
        self.deliver_root_mailbox_locked().await;
    }

    /// [`AcpBackend::deliver_root_mailbox`] for a caller that holds the rewind admission.
    async fn deliver_root_mailbox_locked(&self) {
        if self
            .is_shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
            || self.session_driver.lock().await.prompt_is_busy()
        {
            return;
        }
        let inner = self.orchestrator.inner();
        let Some(root) = inner.directory().with(|registry| registry.root.clone()) else {
            return;
        };
        let items = inner.take_inbox_of(&root).await;
        if items.is_empty() {
            return;
        }
        let combined = super::text::turn_prompt(
            None,
            &items
                .iter()
                .map(|item| super::text::Rendered {
                    from: Some(item.from.clone()),
                    id: item.message_id.clone(),
                    body: item.message.clone(),
                })
                .collect::<Vec<_>>(),
        );
        // Offered to the ledger, which starts it only if no turn of any owner is open at that
        // instant. Refused, the mail stays in the inbox for the next turn boundary.
        let sent = self
            .offer_prompt(
                None,
                QueuedPrompt {
                    event_id: format!("actor-mailbox-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::TaskResults,
                    text: combined.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![nori_protocol::acp::v1::ContentBlock::Text(
                        nori_protocol::acp::v1::TextContent::new(combined),
                    )],
                    display_text: None,
                },
            )
            .await;
        if !sent {
            let ids: Vec<String> = items.into_iter().map(|item| item.message_id).collect();
            inner.restore_inbox(&root, &ids);
        }
    }

    /// Hand the parent agent the results it is owed, in a hidden turn, if it is idle. A busy
    /// parent gets them when its turn ends.
    pub(in crate::backend) async fn deliver_task_results_if_idle(&self) {
        let _admission = self.orchestrator.rewind_admission().await;
        self.deliver_root_mailbox_locked().await;
        // A native child may be waiting while the root is busy on its provider Task call.
        // Its completions must reach that child independently of root prompt admission.
        self.orchestrator.deliver_native_caller_results().await;
        let busy = self.session_driver.lock().await.prompt_is_busy();
        if busy
            || self
                .is_shutting_down
                .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let Some(leased) = self.orchestrator.peek_wake().await else {
            return;
        };
        let fence = leased.fence;
        if let Some(recorder) = self.transcript_recorder.read().await.clone() {
            let payload = serde_json::json!({
                "fence": fence,
                "tasks": leased.wake.delivered.tasks.iter().map(|t| &t.task_id).collect::<Vec<_>>(),
            })
            .to_string();
            if recorder
                .record_command("wake-leased", payload)
                .await
                .is_err()
            {
                self.orchestrator.abort_wake(fence).await;
                return;
            }
        }
        // Offered, not queued: a wake that reached a ledger with a turn open is not delivered, and
        // its lease is given back to be offered again at the next turn boundary. The reply
        // arrives only after the turn has started, so no separate barrier is needed. The offer
        // takes no admission lock, so holding it here cannot wait on the runtime actor.
        let sent = self
            .offer_prompt(
                None,
                QueuedPrompt {
                    event_id: format!("task-results-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::TaskResults,
                    text: leased.wake.text.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![nori_protocol::acp::v1::ContentBlock::Text(
                        nori_protocol::acp::v1::TextContent::new(leased.wake.text),
                    )],
                    display_text: None,
                },
            )
            .await;
        if !sent {
            self.orchestrator.abort_wake(fence).await;
            return;
        }
        if let Some(recorder) = self.transcript_recorder.read().await.clone() {
            let payload = serde_json::json!({ "fence": fence }).to_string();
            if recorder
                .record_command("wake-committed", payload)
                .await
                .is_err()
            {
                self.orchestrator.abort_wake(fence).await;
                return;
            }
        }
        if self.orchestrator.commit_wake(fence).await.is_ok() {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    NoriEvent::TaskResultsDelivered(leased.wake.delivered),
                )))
                .await;
        }
    }
}
