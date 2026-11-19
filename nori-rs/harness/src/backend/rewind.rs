//! Rebind a rewound conversation to a fresh ACP engine session.

use super::*;

impl AcpBackend {
    /// Run a session swap with the ledger fenced: no turn is open and no prompt of any kind can
    /// start until it is over. Prompts that arrive meanwhile wait in the queue, and so does what
    /// was queued before; all of it starts on the session that results.
    ///
    /// A turn that is open when a swap is asked for (a prompt, an agent turn, a retry waiting out
    /// its backoff) is cancelled, and the swap waits for the ledger to see it end. A swap never
    /// happens in the middle of a turn, and it does not begin if the agent will not stop.
    pub(crate) async fn while_fenced<T>(
        &self,
        what: &str,
        swap: impl std::future::Future<Output = Result<T>>,
    ) -> Result<T> {
        self.acquire_fence(what).await?;
        let result = swap.await;
        let _ = self
            .session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::ReleaseFence,
            ))
            .await;
        result
    }

    async fn acquire_fence(&self, what: &str) -> Result<()> {
        const WAIT: std::time::Duration = std::time::Duration::from_secs(25);
        // A turn that is ending by itself (a failure being recorded, a response arriving) is
        // not told to stop.
        const GRACE: std::time::Duration = std::time::Duration::from_millis(300);
        let started = tokio::time::Instant::now();
        let deadline = started + WAIT;
        // The cancel is repeated while waiting: a turn that ended and an agent that went on
        // opened a new one, which a cancel sent earlier did not reach.
        const REPEAT: std::time::Duration = std::time::Duration::from_secs(2);
        let mut last_cancel: Option<tokio::time::Instant> = None;
        loop {
            if self.session_driver.lock().await.try_fence() {
                return Ok(());
            }
            if started.elapsed() >= GRACE && last_cancel.is_none_or(|sent| sent.elapsed() >= REPEAT)
            {
                if last_cancel.is_none() {
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            nori_protocol::NoriEvent::Notice(nori_protocol::Notice {
                                message: format!(
                                    "Stopping the running turn so the session can {what}."
                                ),
                            }),
                        )))
                        .await;
                }
                last_cancel = Some(tokio::time::Instant::now());
                let _ = self
                    .session_event_tx
                    .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                        session_reducer::InboundEvent::CancelSubmit,
                    ))
                    .await;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "Cannot {what}: the agent is still working {}s after it was asked to stop. Stop the agent and try again.",
                WAIT.as_secs()
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Rewind the canonical visible head and replace the opaque ACP session.
    pub(crate) async fn rewind_and_rebind(
        &self,
        expected_session_id: String,
        head: Option<i64>,
    ) -> Result<()> {
        self.rewind_checked(expected_session_id, head, None).await
    }

    /// Rewind for a Pkl program only if its original generation is still current.
    pub(crate) async fn rewind_program(
        &self,
        expected_session_id: String,
        head: Option<i64>,
        expected_generation: i64,
    ) -> Result<()> {
        self.rewind_checked(expected_session_id, head, Some(expected_generation))
            .await
    }

    async fn rewind_checked(
        &self,
        expected_session_id: String,
        head: Option<i64>,
        expected_generation: Option<i64>,
    ) -> Result<()> {
        let _program_admission = self.program_effects.try_write().map_err(|_| {
            anyhow::anyhow!("Cannot rewind while a conversation program effect is running")
        })?;
        if let Some(expected) = expected_generation {
            anyhow::ensure!(
                self.orchestrator.admission_generation() == expected,
                "The conversation was rewound before this program could continue"
            );
        }
        let recorder = self
            .transcript_recorder
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("The conversation has no journal"))?;
        anyhow::ensure!(
            recorder.session_id() == expected_session_id,
            "The conversation changed before rewind"
        );
        let _admission = self.orchestrator.rewind_admission().await;
        anyhow::ensure!(
            !self.orchestrator.has_pending_work().await,
            "Cannot rewind while delegated work is running or its result is pending"
        );
        anyhow::ensure!(
            !self.native_children.has_pending_work().await,
            "Cannot rewind while native child work is running or queued"
        );
        self.while_fenced("rewind", async {
            recorder.flush().await?;
            let context = recorder.projected_context(head).await?;
            self.swap_active_session(session_swap::SessionSwapMode::Rewind {
                head,
                context,
                recorder,
            })
            .await?;
            self.orchestrator.advance_generation();
            // The agent no longer remembers arming its monitors, or what they reported.
            let monitors = self.orchestrator.monitors();
            let _ = monitors.stop("all", monitor::Ending::Rewound).await;
            monitors.take_unread().await;
            Ok(())
        })
        .await?;
        Ok(())
    }
}
