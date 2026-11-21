//! The one way a prompt nobody typed starts: it is offered to the turn ledger.
//!
//! Goal continuations, workflow hooks, delegated results, peer mail and CI verdicts are all
//! decided on a stale view of the session by the time they reach it: the agent may have begun
//! working, or the user may have rewound, since. The ledger is the only arbiter. An offer starts
//! its prompt only if, at that instant, no turn of any owner is open, nothing is queued, no
//! session swap is in flight, and (for background work) the conversation generation that
//! created it is still current. Otherwise it is refused and the offerer keeps what it owed, to
//! offer again at the next turn boundary. It is never queued behind another turn, where it could
//! outlive the reason it was sent or be stranded by a cancelled stop reason.

use super::AcpBackend;
use super::session_reducer::InboundEvent;
use super::session_runtime_driver::SessionRuntimeInput;
use crate::normalized::session_runtime::QueuedPrompt;

impl AcpBackend {
    /// Offer a prompt to the ledger. `generation` is the conversation generation a background
    /// continuation belongs to; an offer from an older one is refused. Returns whether the
    /// prompt started.
    pub(super) async fn offer_prompt(&self, generation: Option<i64>, prompt: QueuedPrompt) -> bool {
        // The runtime actor is the ledger's only writer and cannot wait on itself.
        if super::session_runtime_driver::IN_RUNTIME_ACTOR
            .try_with(|()| ())
            .is_ok()
        {
            return self.admit_offer(generation, prompt).await;
        }
        let (admitted, reply) = tokio::sync::oneshot::channel();
        let offer = SessionRuntimeInput::Offer {
            generation,
            prompt: Box::new(prompt),
            admitted,
        };
        if self.session_event_tx.send(offer).await.is_err() {
            return false;
        }
        reply.await.unwrap_or(false)
    }

    /// [`AcpBackend::apply_session_event`] behind a boxed boundary: applying an event can end a
    /// turn, whose boundary can offer a prompt, which applies an event. The box is the one place
    /// that cycle is allowed to close.
    pub(super) fn apply_session_event_boxed(
        &self,
        event: InboundEvent,
    ) -> futures::future::BoxFuture<'_, ()> {
        Box::pin(self.apply_session_event(event))
    }

    /// Decide an offer. Only the runtime actor calls this, so nothing can change the ledger
    /// between the check and the prompt starting. It takes no lock a caller might hold while it
    /// waits for the answer: a rewind protects itself by fencing the ledger, which makes the
    /// session busy, and by advancing the generation, which refuses what it outdated.
    pub(super) async fn admit_offer(&self, generation: Option<i64>, prompt: QueuedPrompt) -> bool {
        if generation
            .is_some_and(|generation| generation != self.orchestrator.admission_generation())
            || self.session_driver.lock().await.prompt_is_busy()
        {
            return false;
        }
        self.apply_session_event_boxed(InboundEvent::PromptSubmit(prompt))
            .await;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn queued_old_continuations_never_reach_the_rewound_provider() {
        use crate::normalized::session_runtime::QueuedPromptKind;
        use nori_protocol::acp::v1 as acp;

        let home = tempfile::tempdir().expect("session home");
        let wire = home.path().join("wire");
        let config = crate::config::NoriConfig {
            active_agent: "mock-model".to_owned(),
            cwd: home.path().to_path_buf(),
            nori_home: home.path().to_path_buf(),
            acp_proxy: nori_config::AcpProxyConfig {
                enabled: true,
                log_dir: wire.clone(),
            },
            ..Default::default()
        };
        let prepared = crate::runtime::prepare_agent(crate::runtime::AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "continuation-guard-test".to_owned(),
            session_context: None,
            initial_context: None,
        })
        .await
        .expect("prepare mock agent");
        let (events, mut receiver) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move { while receiver.recv().await.is_some() {} });
        let backend = AcpBackend::spawn(prepared, events)
            .await
            .expect("start mock backend");
        let recorder = backend
            .transcript_recorder
            .read()
            .await
            .clone()
            .expect("journal");
        let old = backend.orchestrator.admission_generation();
        backend
            .rewind_and_rebind(recorder.session_id().to_owned(), None)
            .await
            .expect("rewind session");
        let prompt = |kind, text: &str| QueuedPrompt {
            event_id: text.to_owned(),
            kind,
            text: text.to_owned(),
            prompt_meta: None,
            original_content: Vec::new(),
            content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
            context: Default::default(),
            display_text: None,
        };
        for (kind, text) in [
            (QueuedPromptKind::GoalContinuation, "discarded goal"),
            (QueuedPromptKind::WorkflowHook, "discarded workflow"),
        ] {
            assert!(!backend.offer_prompt(Some(old), prompt(kind, text)).await);
        }
        assert!(
            backend
                .offer_prompt(
                    Some(backend.orchestrator.admission_generation()),
                    prompt(QueuedPromptKind::WorkflowHook, "current workflow"),
                )
                .await
        );
        let wire_text = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let logs = std::fs::read_dir(&wire)
                    .ok()
                    .into_iter()
                    .flatten()
                    .filter_map(Result::ok)
                    .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                    .collect::<String>();
                if logs.contains("current workflow") {
                    break logs;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("current continuation reaches provider");
        assert!(!wire_text.contains("discarded goal"), "{wire_text}");
        assert!(!wire_text.contains("discarded workflow"), "{wire_text}");
        backend
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .expect("shutdown backend");
    }
}
