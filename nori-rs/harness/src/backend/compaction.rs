//! Summarize-and-swap `/compact`, for agents that do not compact on their own (see
//! `crate::compact` for the prompt, the checks, and the checkpoint).

use std::sync::atomic::Ordering;

use nori_protocol::NoriEvent;

use super::*;
use crate::backend::session_runtime_driver::CompletedTurn;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

fn compaction_prompt(id: &str, text: &str) -> QueuedPrompt {
    QueuedPrompt {
        event_id: id.to_string(),
        kind: QueuedPromptKind::Compact,
        text: text.to_string(),
        prompt_meta: None,
        original_content: Vec::new(),
        content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
        display_text: None,
    }
}

impl AcpBackend {
    /// Ask the agent, in the live session, for the summary (`text` is the prompt).
    pub(super) async fn submit_compaction_prompt(&self, id: &str, text: &str) {
        let _ = self
            .session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::PromptSubmit(compaction_prompt(id, text)),
            ))
            .await;
    }

    /// The summary turn ended. The ledger fenced the queue when it did, so nothing has started
    /// since: what was queued, and every prompt submitted meanwhile, is still waiting. Decide
    /// whether to swap, and then release the fence so the queue starts on whichever session
    /// results. Every way out releases it.
    ///
    /// Invariant: a compaction either swaps completely, or leaves the old session exactly as it
    /// was and says so. Queued prompts neither start on a half-swapped session nor are lost, and
    /// the checkpoint reaches whichever prompt is first to run on the new session, queued or not.
    pub(super) async fn finish_compaction(&self, completed_turn: &CompletedTurn) {
        let remind = self.conclude_compaction(completed_turn).await;
        if remind {
            // The reminder goes ahead of what was queued: it finishes the same compaction.
            self.apply_session_event_boxed(session_reducer::InboundEvent::PromptSteer(
                compaction_prompt(&generate_id(), crate::compact::NUDGE),
            ))
            .await;
        }
        self.apply_session_event_boxed(session_reducer::InboundEvent::ReleaseFence)
            .await;
    }

    /// Check the summary and swap to a new session that opens with the checkpoint. Returns
    /// whether the agent should be reminded of the template once. On any failure the old
    /// session stays, and the user is told why.
    async fn conclude_compaction(&self, completed_turn: &CompletedTurn) -> bool {
        let summary = completed_turn
            .last_agent_message
            .clone()
            .unwrap_or_default();
        if completed_turn.stop_reason != acp::StopReason::EndTurn {
            self.compact_nudged.store(false, Ordering::Release);
            self.notice("Compaction stopped before the agent wrote its summary; the conversation goes on as it was.")
                .await;
            return false;
        }
        if !crate::compact::fills_template(&summary) {
            if !self.compact_nudged.swap(true, Ordering::AcqRel) {
                return true;
            }
            self.compact_nudged.store(false, Ordering::Release);
            self.notice("Compaction failed: the agent did not write its summary in the template, so the conversation goes on as it was.")
                .await;
            return false;
        }
        self.compact_nudged.store(false, Ordering::Release);
        // The summary describes a session the agent is still working in: swapping it away
        // would orphan what it is doing and the requests it waits on.
        if self.session_driver.lock().await.has_open_turn() {
            self.notice("Compaction was skipped: the agent started working again before it could swap, so the conversation goes on as it was.")
                .await;
            return false;
        }
        let recent = self.recent_context().await;
        *self.pending_compact_summary.lock().await =
            Some(crate::compact::checkpoint(&summary, recent.as_deref()));
        if let Err(error) = self
            .swap_active_session(session_swap::SessionSwapMode::NewAfterCompact)
            .await
        {
            warn!("Failed to create new session after compact: {error}");
            self.pending_compact_summary.lock().await.take();
            self.notice(&format!(
                "Compaction failed: no new session could be started ({error}); the conversation goes on as it was."
            ))
            .await;
            return false;
        }
        self.forward_client_event(crate::normalized::ClientEvent::ContextCompacted(
            crate::normalized::ContextCompacted {
                summary: Some(summary),
            },
        ))
        .await;
        self.notice("Heads up: Long conversations and multiple compactions can cause the model to be less accurate. Start a new conversation when possible to keep conversations small and targeted.")
            .await;
        false
    }

    /// The newest exchanges of this session's transcript, for the checkpoint.
    pub(super) async fn recent_context(&self) -> Option<String> {
        let recorder = self.transcript_recorder.read().await.clone()?;
        recorder.flush().await.ok()?;
        let transcript = crate::transcript::TranscriptLoader::new(self.nori_home.clone())
            .load_transcript(recorder.project_id(), recorder.session_id())
            .await
            .ok()?;
        crate::compact::recent_context(&transcript)
    }

    pub(super) async fn notice(&self, message: &str) {
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                nori_protocol::Notice {
                    message: message.to_string(),
                },
            ))))
            .await;
    }
}
