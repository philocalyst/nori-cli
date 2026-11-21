use super::*;

impl AcpBackend {
    pub(crate) async fn fork_history(
        &self,
        context: String,
        source: Option<(String, i64, i64)>,
    ) -> anyhow::Result<()> {
        let generation = self.orchestrator.admission_generation();
        let _admission = self.orchestrator.rewind_admission().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the fork started"
        );
        let (context, entries) = if let Some((session_id, start, end)) = &source {
            let recorder = self
                .transcript_recorder
                .read()
                .await
                .clone()
                .ok_or_else(|| anyhow::anyhow!("The source conversation has no event log"))?;
            let records = recorder
                .projected_fork_records(session_id, *start, *end)
                .await?;
            let entries =
                crate::transcript::fork_context::replay_entries_from_records(&records, session_id);
            (
                crate::transcript::fork_context::context_from_records(
                    records,
                    crate::context_budget::ContextBudget::for_agent(&self.agent_name),
                )?,
                entries,
            )
        } else {
            (context, Vec::new())
        };
        self.orchestrator
            .fork_history(context, source, entries)
            .await
            .map_err(anyhow::Error::msg)
    }

    /// Flush the active transcript recorder to disk (write barrier). A
    /// missing recorder (transcripts disabled) flushes trivially.
    pub(crate) async fn flush_transcript(&self) -> Result<()> {
        if let Some(recorder) = self.transcript_recorder.read().await.clone() {
            recorder.flush().await?;
        }
        Ok(())
    }

    pub(crate) async fn cancel(&self) -> Result<()> {
        // A cancel ends the turn and nothing else: delegated actors are independent and run on,
        // and their results arrive at the next boundary, whether or not a turn is open. Stopping
        // them is its own command (`stop_subagents`); stopping the agent and quitting stop them
        // as well.
        self.session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::CancelSubmit,
            ))
            .await
            .map_err(|_| anyhow::anyhow!("session runtime closed"))
    }

    /// Stop every task the session's agents delegated, on the user's explicit request.
    pub(crate) async fn stop_subagents(&self) -> Result<()> {
        self.orchestrator.stop().await;
        Ok(())
    }

    pub(crate) async fn cancel_request(&self, request_id: acp::RequestId) -> Result<()> {
        self.session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::CancelSubmitFor { request_id },
            ))
            .await
            .map_err(|_| anyhow::anyhow!("session runtime closed"))
    }

    /// Stop a runaway agent: tear down its connection. Whatever turn is open ends exactly as if
    /// the connection had died (its requests answered as cancelled, its unfinished tool calls
    /// closed, the turn failed), the delegated tasks stop, and the session ends as
    /// `ConnectionLost`, so it can be resumed from its journal.
    pub(crate) async fn stop_agent(&self) -> Result<()> {
        self.orchestrator.stop().await;
        self.connection_lost().await;
        self.teardown(true, Some(std::time::Duration::from_secs(2)))
            .await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionEnded(nori_protocol::SessionEnded {
                    reason: nori_protocol::SessionEndReason::ConnectionLost,
                    message: Some(format!(
                        "You stopped {}. Resume this session to continue the conversation.",
                        self.agent_name
                    )),
                }),
            )))
            .await;
        Ok(())
    }

    pub(crate) async fn shutdown(&self, child_grace: std::time::Duration) -> Result<()> {
        self.orchestrator.shutdown().await;
        // A turn still open when the user quits ends as the connection's loss would end it: its
        // unfinished tool calls are closed and the idle phase published, in the stream and the
        // journal, before the session ends. Bounded: shutdown never waits on a stuck actor.
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(2), self.connection_lost()).await;
        self.teardown(true, Some(child_grace)).await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionEnded(nori_protocol::SessionEnded {
                    reason: nori_protocol::SessionEndReason::Shutdown,
                    message: None,
                }),
            )))
            .await;
        Ok(())
    }

    /// Stop this backend because the runtime replaced it with another agent's: the same as
    /// shutting down, but the conversation goes on, so the session does not end.
    pub(crate) async fn retire(&self, child_grace: std::time::Duration) {
        self.orchestrator.shutdown().await;
        self.teardown(true, Some(child_grace)).await;
    }

    pub(super) async fn teardown(
        &self,
        cancel_session: bool,
        child_grace: Option<std::time::Duration>,
    ) {
        self.withdraw_all_questions().await;
        self.native_children.shutdown().await;
        self.is_shutting_down
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(abort) = self.cancel_timeout_abort.lock().await.take() {
            abort.abort();
        }
        if let Some(abort) = self.prompt_task_abort.lock().await.take() {
            abort.abort();
        }
        if let Some(abort) = self.runtime_task_abort.lock().await.take() {
            abort.abort();
        }
        if cancel_session {
            let _ = self.connection.cancel(&*self.session_id.read().await).await;
        }

        if !self.session_end_hooks.is_empty() {
            let results =
                crate::hooks::execute_hooks(&self.session_end_hooks, self.script_timeout).await;
            route_hook_results(&results, &self.backend_event_tx, None).await;
        }
        if let Some(handle) = crate::hooks::execute_hooks_fire_and_forget(
            self.async_session_end_hooks.clone(),
            self.script_timeout,
            HashMap::new(),
        ) && let Err(error) = handle.await
        {
            warn!("Async session_end hook task panicked: {error}");
        }
        if let Some(child_grace) = child_grace {
            self.connection.shutdown_with_grace(child_grace).await;
        } else {
            self.connection.shutdown().await;
        }
        if cancel_session && let Some(abort) = self.relay_task_abort.lock().await.take() {
            abort.abort();
        }
    }

    pub(crate) async fn add_history(&self, text: String) -> Result<()> {
        let conversation_id = *self.conversation_id.read().await;
        crate::message_history::append_entry(
            &text,
            &conversation_id,
            &self.nori_home,
            self.history_persistence,
        )
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn history_entry(
        &self,
        log_id: i64,
        offset: i64,
    ) -> Result<Option<crate::HistoryEntry>> {
        let log_id =
            u64::try_from(log_id).map_err(|_| anyhow::anyhow!("invalid history log id"))?;
        let offset =
            usize::try_from(offset).map_err(|_| anyhow::anyhow!("invalid history offset"))?;
        let nori_home = self.nori_home.clone();
        tokio::task::spawn_blocking(move || {
            crate::message_history::lookup(log_id, offset, &nori_home)
        })
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn search_history(
        &self,
        max_results: i64,
    ) -> Result<Vec<crate::HistoryEntry>> {
        let max_results = usize::try_from(max_results)
            .map_err(|_| anyhow::anyhow!("invalid history result limit"))?;
        let nori_home = self.nori_home.clone();
        tokio::task::spawn_blocking(move || {
            crate::message_history::search_entries(&nori_home, max_results)
        })
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn custom_prompts(&self) -> Vec<crate::CustomPrompt> {
        crate::custom_prompts::discover_prompts_in(&commands_dir(&self.nori_home)).await
    }

    pub(crate) async fn compact(&self) -> Result<()> {
        self.handle_compact(&generate_id()).await
    }

    /// Whether the connected agent advertises a slash command with the given
    /// name in the active session runtime.
    pub(super) async fn advertises_command(&self, name: &str) -> bool {
        self.session_driver.lock().await.advertises_command(name)
    }

    pub(crate) async fn undo(&self) -> Result<()> {
        self.connection
            .cancel(&*self.session_id.read().await)
            .await
            .ok();
        crate::undo::handle_undo(&self.backend_event_tx, &generate_id(), &self.edit_journal).await;
        Ok(())
    }

    pub(crate) async fn undo_snapshots(&self) -> Vec<crate::UndoSnapshot> {
        self.edit_journal.list()
    }

    pub(crate) async fn undo_to(&self, index: i64) -> Result<()> {
        self.connection
            .cancel(&*self.session_id.read().await)
            .await
            .ok();
        crate::undo::handle_undo_to(
            &self.backend_event_tx,
            &generate_id(),
            &self.edit_journal,
            index,
        )
        .await;
        Ok(())
    }

    pub(crate) async fn run_user_shell(&self, command: String) -> Result<()> {
        let backend_event_tx = self.backend_event_tx.clone();
        let session_cell = Arc::clone(&self.user_bash_session);
        let cwd = self.cwd.clone();
        let operation_id = generate_id();
        tokio::spawn(async move {
            user_shell::run_user_shell_command(
                &backend_event_tx,
                &session_cell,
                &operation_id,
                &cwd,
                command,
            )
            .await;
        });
        Ok(())
    }

    pub(crate) fn set_approval_policy(&self, _policy: nori_config::AskForApproval) {
        let _ = self
            .approval_policy_tx
            .send(nori_config::AskForApproval::Never);
    }

    pub(crate) async fn respond_to_agent(
        &self,
        request_id: acp::RequestId,
        response: Result<acp::ClientResponse, acp::Error>,
    ) -> Result<()> {
        let pending = {
            let mut pending = self.pending_approvals.lock().await;
            let Some(index) = pending
                .iter()
                .position(|pending| pending.request.request_id == request_id)
            else {
                anyhow::bail!("no delegated ACP request is pending for {request_id}");
            };
            pending.remove(index)
        };
        let sent = pending
            .request
            .response_tx
            .send(response)
            .map_err(|_| anyhow::anyhow!("ACP request responder closed"));
        // The agent is no longer waiting on this: the ledger stops holding its turn open for it.
        let _ = self
            .session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::RequestAnswered {
                    request_id: request_id.to_string(),
                },
            ))
            .await;
        sent
    }

    pub(crate) async fn submit_prompt(
        &self,
        content: Vec<acp::ContentBlock>,
        prompt_meta: Option<acp::Meta>,
        admission: PromptAdmission,
        response_tx: oneshot::Sender<Result<acp::RequestId>>,
    ) {
        // Nobody cancels this admission; the sender lives until it is decided, since a dropped one
        // reads as a cancellation.
        let (_never_cancelled, cancellation) = tokio::sync::watch::channel(false);
        self.submit_prompt_cancellable(content, prompt_meta, admission, cancellation, response_tx)
            .await;
    }

    pub(crate) async fn submit_prompt_cancellable(
        &self,
        content: Vec<acp::ContentBlock>,
        prompt_meta: Option<acp::Meta>,
        admission: PromptAdmission,
        cancellation: tokio::sync::watch::Receiver<bool>,
        response_tx: oneshot::Sender<Result<acp::RequestId>>,
    ) {
        let has_content = content.iter().any(|block| match block {
            acp::ContentBlock::Text(text) => !text.text.is_empty(),
            _ => true,
        });
        if !has_content {
            let _ = response_tx.send(Err(anyhow::anyhow!("prompt content is empty")));
            return;
        }
        // Results a stop held back are handed over once this turn ends.
        self.orchestrator.release_held().await;

        if admission == PromptAdmission::RejectIfBusy {
            let (barrier_tx, barrier_rx) = oneshot::channel();
            if self
                .session_event_tx
                .send(session_runtime_driver::SessionRuntimeInput::Barrier {
                    response_tx: barrier_tx,
                })
                .await
                .is_err()
                || barrier_rx.await.is_err()
            {
                let _ = response_tx.send(Err(anyhow::anyhow!(
                    "ACP session runtime closed before prompt admission"
                )));
                return;
            }
            if self.session_driver.lock().await.prompt_is_busy() {
                let _ = response_tx.send(Err(PromptBusy.into()));
                return;
            }
        }

        let id = generate_id();
        self.pending_prompt_submissions
            .lock()
            .await
            .insert(id.clone(), response_tx);
        if let Err(error) = self
            .handle_prompt(content, &id, prompt_meta, admission, cancellation)
            .await
            && let Some(response_tx) = self.pending_prompt_submissions.lock().await.remove(&id)
        {
            let _ = response_tx.send(Err(error));
        }
    }

    /// Prioritize a queued prompt without stopping delegated child sessions.
    pub(crate) async fn promote_next_queued(&self) -> Result<()> {
        let generation = self.orchestrator.admission_generation();
        let _admission = self.orchestrator.rewind_admission().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before queue promotion"
        );
        self.session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                session_reducer::InboundEvent::PromoteQueued,
            ))
            .await
            .map_err(|_| anyhow::anyhow!("session runtime closed"))?;
        let (response_tx, response_rx) = oneshot::channel();
        self.session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Barrier { response_tx })
            .await
            .map_err(|_| anyhow::anyhow!("session runtime closed"))?;
        response_rx
            .await
            .map_err(|_| anyhow::anyhow!("session runtime closed"))?;
        Ok(())
    }

    /// Handle the /compact operation.
    ///
    /// If the agent advertises a native `compact` slash command, forward
    /// `/compact` as an ordinary in-session turn: the agent compacts its own
    /// context with no session swap and no summary re-injection.
    ///
    /// Otherwise fall back to summarize-and-swap: send a summarization prompt,
    /// capture the agent's summary into `pending_compact_summary` (prepended to
    /// the next prompt), create a fresh session, and swap to it.
    pub(super) async fn handle_compact(&self, id: &str) -> Result<()> {
        use crate::compact::SUMMARIZATION_PROMPT;

        // If the agent advertises a native `compact` command, forward `/compact`
        // as an ordinary in-session turn instead of summarize-and-swap.
        if self.advertises_command("compact").await {
            let _ = self
                .session_event_tx
                .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                    session_reducer::InboundEvent::PromptSubmit(
                        crate::normalized::session_runtime::QueuedPrompt {
                            event_id: id.to_string(),
                            kind:
                                crate::normalized::session_runtime::QueuedPromptKind::NativeCompact,
                            text: "/compact".to_string(),
                            prompt_meta: None,
                            original_content: Vec::new(),
                            content: vec![acp::ContentBlock::Text(acp::TextContent::new(
                                "/compact",
                            ))],
                            context: Default::default(),
                            display_text: None,
                        },
                    ),
                ))
                .await;
            return Ok(());
        }

        // A new compaction starts with its reminder unused.
        self.compact_nudged
            .store(false, std::sync::atomic::Ordering::Release);
        self.submit_compaction_prompt(id, SUMMARIZATION_PROMPT)
            .await;

        Ok(())
    }

    /// Get the current ACP session config snapshot from the connection.
    pub fn config_options(&self) -> Vec<acp::SessionConfigOption> {
        self.connection.config_options()
    }

    /// Get the current session ID.
    ///
    /// Note: This clones the session ID since it may be replaced during /compact.
    pub async fn session_id(&self) -> acp::SessionId {
        self.session_id.read().await.clone()
    }

    /// Get a reference to the underlying ACP connection.
    ///
    /// This provides access to low-level ACP operations like session controls.
    pub fn connection(&self) -> &Arc<AcpConnection> {
        &self.connection
    }

    /// Set an ACP session config option for the current session.
    pub async fn set_config_option(
        &self,
        config_id: impl Into<acp::SessionConfigId>,
        value: impl Into<acp::SessionConfigValueId>,
    ) -> Result<()> {
        // The id is copied out: the lock is not held while the agent answers.
        let session_id = self.session_id.read().await.clone();
        self.connection
            .set_config_option(&session_id, config_id, value)
            .await
    }
}
