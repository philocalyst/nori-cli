use super::*;

impl AcpBackend {
    /// Submit schema-native ACP content through Nori's prompt lifecycle.
    pub(super) async fn handle_prompt(
        &self,
        content: Vec<acp::ContentBlock>,
        id: &str,
        mut prompt_meta: Option<acp::Meta>,
        admission: PromptAdmission,
        mut cancellation: tokio::sync::watch::Receiver<bool>,
    ) -> Result<()> {
        let prompt_text = content
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .fold(String::new(), |mut prompt_text, text| {
                if !prompt_text.is_empty() {
                    prompt_text.push('\n');
                }
                prompt_text.push_str(text);
                prompt_text
            });

        // Inline workflow clauses are for Nori, not the agent: the text goes on without them.
        let (workflow_text, workflow_rules) = {
            let workflow = self.workflow_for_prompt(&prompt_text);
            tokio::pin!(workflow);
            tokio::select! {
                biased;
                _ = async {
                    while !*cancellation.borrow_and_update() {
                        if cancellation.changed().await.is_err() {
                            break;
                        }
                    }
                } => anyhow::bail!("prompt admission was canceled while waiting for policy"),
                result = &mut workflow => result,
            }
        };
        let (prompt_text, content) = if workflow_text == prompt_text {
            (prompt_text, content)
        } else {
            let mut rest: Vec<acp::ContentBlock> = content
                .into_iter()
                .filter(|block| !matches!(block, acp::ContentBlock::Text(_)))
                .collect();
            if !workflow_text.trim().is_empty() {
                rest.insert(
                    0,
                    acp::ContentBlock::Text(acp::TextContent::new(workflow_text.clone())),
                );
            }
            (workflow_text, rest)
        };

        if prompt_text.trim().is_empty() && content.is_empty() {
            return Ok(());
        }

        // For image-only prompts, use a placeholder for downstream consumers
        // (hooks, transcript, summary, snapshot labels) that expect non-empty text.
        let display_text = if prompt_text.is_empty() && !content.is_empty() {
            "[attachment]".to_string()
        } else {
            prompt_text.clone()
        };

        // Execute pre_user_prompt hooks before sending the prompt
        if !self.pre_user_prompt_hooks.is_empty() {
            let env_vars = HashMap::from([
                ("NORI_HOOK_EVENT".to_string(), "pre_user_prompt".to_string()),
                ("NORI_HOOK_PROMPT_TEXT".to_string(), display_text.clone()),
            ]);
            let results = crate::hooks::execute_hooks_with_env(
                &self.pre_user_prompt_hooks,
                self.script_timeout,
                &env_vars,
            )
            .await;
            route_hook_results(
                &results,
                &self.backend_event_tx,
                Some(&self.pending_hook_context),
            )
            .await;
        }

        // Fire-and-forget async pre_user_prompt hooks
        if !self.async_pre_user_prompt_hooks.is_empty() {
            let env_vars = HashMap::from([
                ("NORI_HOOK_EVENT".to_string(), "pre_user_prompt".to_string()),
                ("NORI_HOOK_PROMPT_TEXT".to_string(), display_text.clone()),
            ]);
            let _ = crate::hooks::execute_hooks_fire_and_forget(
                self.async_pre_user_prompt_hooks.clone(),
                self.script_timeout,
                env_vars,
            );
        }

        // On first prompt, spawn a fire-and-forget summarization task.
        // Skip for mock models (debug-only test agents) since they don't
        // produce meaningful summaries.
        {
            let mut is_first = self.is_first_prompt.lock().await;
            if *is_first {
                *is_first = false;
                if !(cfg!(debug_assertions) && self.agent_name.starts_with("mock-")) {
                    self.announce_derived_title(&prompt_text).await;
                }
                let skip_summary = cfg!(debug_assertions) && self.agent_name.starts_with("mock-");
                if !skip_summary && (self.prompt_summary_enabled || self.auto_worktree.is_enabled())
                {
                    let event_tx = self.backend_event_tx.clone();
                    let agent_name = self.agent_name.clone();
                    let cwd = self.cwd.clone();
                    let prompt_for_summary = display_text.clone();
                    let auto_worktree = self.auto_worktree;
                    let auto_worktree_repo_root = self.auto_worktree_repo_root.clone();
                    let acp_proxy = self.acp_proxy.clone();
                    tokio::spawn(async move {
                        if let Err(e) = run_prompt_summary(
                            &event_tx,
                            &agent_name,
                            &cwd,
                            &prompt_for_summary,
                            auto_worktree,
                            auto_worktree_repo_root.as_deref(),
                            acp_proxy,
                        )
                        .await
                        {
                            debug!("Prompt summary failed (non-fatal): {e}");
                        }
                    });
                }
            }
        }

        // Save prompt text for post_user_prompt hooks (before it gets moved)
        let prompt_text_for_hooks = display_text;

        // The caller's ACP content stays as given; what Nori says along with it is put around it
        // when the prompt is sent (`prompt_context`). Only what is this prompt's own is recorded
        // now, so a prompt that is refused, dropped or never sent takes nothing from the session.
        let echo_id = prompt_meta
            .as_ref()
            .and_then(|meta| meta.get(nori_protocol::PROMPT_ECHO_ID_META_KEY))
            .and_then(serde_json::Value::as_str)
            .unwrap_or(id)
            .to_string();
        prompt_meta.get_or_insert_default().insert(
            nori_protocol::PROMPT_ECHO_ID_META_KEY.to_string(),
            serde_json::Value::String(echo_id),
        );
        let original_content = content.clone();

        let (phase_before_submit, active_request_id_before_submit, queue_len_before_submit) = {
            let driver = self.session_driver.lock().await;
            (
                driver.phase_label(),
                driver.active_request_id(),
                driver.queue_len(),
            )
        };
        debug!(
            target: "acp_event_flow",
            event_id = id,
            phase_before_submit,
            active_request_id_before_submit = active_request_id_before_submit
                .as_deref()
                .unwrap_or("<none>"),
            queue_len_before_submit,
            prompt_text_len = prompt_text.len(),
            content_blocks = content.len(),
            "Accepted user prompt into ACP backend"
        );

        let prompt = crate::normalized::session_runtime::QueuedPrompt {
            event_id: id.to_string(),
            kind: crate::normalized::session_runtime::QueuedPromptKind::User,
            text: prompt_text,
            prompt_meta,
            original_content,
            content,
            context: Box::new(super::prompt_context::PromptContext {
                fixed: super::prompt_context::StaticContext {
                    workflow_rules,
                    ..Default::default()
                },
                ..Default::default()
            }),
            display_text: Some(prompt_text_for_hooks),
        };
        let event = match admission {
            PromptAdmission::Queue => session_reducer::InboundEvent::PromptSubmit(prompt),
            PromptAdmission::Steer => session_reducer::InboundEvent::PromptSteer(prompt),
            PromptAdmission::RejectIfBusy => {
                session_reducer::InboundEvent::PromptSubmitIfIdle(prompt)
            }
        };
        self.session_event_tx
            .send(session_runtime_driver::SessionRuntimeInput::Reducer(event))
            .await
            .map_err(|_| anyhow::anyhow!("ACP session runtime closed during prompt submission"))?;

        Ok(())
    }
}
