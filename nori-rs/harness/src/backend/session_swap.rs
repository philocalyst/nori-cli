//! Active-session swapping shared by summarize-and-swap `/compact` and
//! branch-at-head `/fork`.
//!
//! Both operations replace the active ACP session with a fresh one obtained
//! from the agent (a brand-new session after compaction, or a fork of the
//! current head), re-registering the backend-owned MCP servers and
//! rebroadcasting capabilities for the new session.

use super::*;

use crate::normalized::ClientEvent;

/// A setting of the session being replaced, to apply to the one replacing it.
struct CarriedSetting {
    model: bool,
    id: acp::SessionConfigId,
    name: String,
    value: String,
}

/// How [`AcpBackend::swap_active_session`] obtains the replacement session id.
pub(super) enum SessionSwapMode {
    /// Create a brand-new session (summarize-and-swap `/compact`).
    NewAfterCompact,
    /// Start a fresh engine after moving the canonical visible head.
    Rewind {
        head: Option<i64>,
        context: String,
        recorder: Arc<TranscriptRecorder>,
    },
    /// Fork the current head into a new session (branch-at-head `/fork`).
    ForkFromHead { from: acp::SessionId },
}

impl AcpBackend {
    /// Replace the active ACP session with a new one, assembling MCP servers,
    /// re-registering the backend-owned `nori-client` server, committing it,
    /// swapping the active session id, and rebroadcasting capabilities.
    ///
    /// On failure the goal-MCP connected flag is rolled back and the error is
    /// returned to the caller.
    ///
    /// Every swap, whatever its mode, runs with the turn ledger fenced (see
    /// [`AcpBackend::while_fenced`]; a finished summary turn leaves the fence up for
    /// compaction): no turn is open and no prompt can start while the session is replaced.
    pub(super) async fn swap_active_session(
        &self,
        mode: SessionSwapMode,
    ) -> Result<acp::SessionId> {
        anyhow::ensure!(
            self.session_driver.lock().await.is_fenced(),
            "internal error: a session swap must hold the turn ledger's fence"
        );
        // The settings in force (model, mode, effort, ...) go with the conversation to the session
        // that replaces it: a compaction or a rewind must not quietly drop plan mode or the model
        // the person picked. The model goes first, since it decides which other settings exist.
        let mut carried: Vec<CarriedSetting> = self
            .connection
            .config_options()
            .into_iter()
            .filter_map(|option| match option.kind {
                acp::SessionConfigKind::Select(select) => Some(CarriedSetting {
                    model: option.category == Some(acp::SessionConfigOptionCategory::Model),
                    id: option.id,
                    name: option.name,
                    value: select.current_value.to_string(),
                }),
                _ => None,
            })
            .collect();
        carried.sort_by_key(|setting| !setting.model);
        let cwd = self.cwd.clone();
        let mut mcp_servers = crate::connection::mcp::to_acp_mcp_servers(
            &self.mcp_servers,
            self.mcp_oauth_credentials_store_mode,
        );
        nudox::add_to(&mut mcp_servers, &cwd, &self.mcp_servers, self.nudox).await;
        let (previous_goal_mcp_connected, host_tools) = {
            let goal_mcp_http_server = self.goal_mcp_http_server.lock().await;
            match goal_mcp_http_server.as_ref() {
                // The new session reconnects to the MCP server, which marks it connected again.
                Some(nori_client_mcp::NoriClient::Mcp(server)) => {
                    mcp_servers.push(server.as_mcp_server());
                    (
                        Some(self.goal_mcp_connected.swap(false, Ordering::Relaxed)),
                        false,
                    )
                }
                // The connection hands host tools to every session it opens.
                Some(nori_client_mcp::NoriClient::Host { .. }) => {
                    (Some(self.goal_mcp_connected.load(Ordering::Relaxed)), true)
                }
                None => (None, false),
            }
        };
        let pending_nori_client_server = if previous_goal_mcp_connected.is_some() {
            None
        } else {
            match nori_client_mcp::register_for_session(
                &self.connection,
                &mut mcp_servers,
                Arc::clone(&self.thread_goal_state),
                self.backend_event_tx.clone(),
                Arc::clone(&self.goal_mcp_connected),
                self.orchestrator.clone(),
                nori_client_mcp::Workspace {
                    cwd: self.cwd.clone(),
                    workflow: Arc::clone(&self.workflow),
                },
            )
            .await
            {
                Ok(server) => server,
                Err(err) => {
                    warn!("Failed to register goal MCP server during session swap: {err}");
                    None
                }
            }
        };
        let nori_client_advertised = host_tools
            || pending_nori_client_server
                .as_ref()
                .is_some_and(nori_client_mcp::PendingNoriClientServer::is_host)
            || mcp_servers.iter().any(|server| {
            matches!(
                server,
                acp::McpServer::Http(acp::McpServerHttp { name, .. })
                    | acp::McpServer::Stdio(acp::McpServerStdio { name, .. }) if name == "nori-client"
            )
        });

        let swap_result = match &mode {
            SessionSwapMode::NewAfterCompact | SessionSwapMode::Rewind { .. } => {
                self.connection.create_session(&cwd, mcp_servers).await
            }
            SessionSwapMode::ForkFromHead { from } => {
                self.connection
                    .fork_session(&from.to_string(), &cwd, mcp_servers)
                    .await
            }
        };

        match swap_result {
            Ok(new_session_id) => {
                let mut kept_default = Vec::new();
                for setting in carried {
                    let current = self
                        .connection
                        .config_options()
                        .into_iter()
                        .find(|option| option.id == setting.id);
                    let result = match current {
                        Some(option) if matches!(&option.kind, acp::SessionConfigKind::Select(select) if select.current_value.to_string() == setting.value) =>
                        {
                            continue;
                        }
                        Some(option) => {
                            self.connection
                                .set_config_option(
                                    &new_session_id,
                                    option.id,
                                    setting.value.clone(),
                                )
                                .await
                        }
                        None => Err(anyhow::anyhow!(
                            "the replacement session does not offer {}",
                            setting.name
                        )),
                    };
                    let Err(error) = result else { continue };
                    // A rewind continues on the model it rewound to, or not at all.
                    if setting.model && matches!(&mode, SessionSwapMode::Rewind { .. }) {
                        if let Some(previous) = previous_goal_mcp_connected {
                            self.goal_mcp_connected.store(previous, Ordering::Relaxed);
                        }
                        return Err(error);
                    }
                    kept_default.push(format!("{} ({error})", setting.name));
                }
                if !kept_default.is_empty() {
                    self.notice(&format!(
                        "The new session could not take over every setting, so these are the agent's defaults now: {}.",
                        kept_default.join("; ")
                    ))
                    .await;
                }
                if let SessionSwapMode::Rewind {
                    head,
                    context,
                    recorder,
                } = &mode
                {
                    if let Err(error) = recorder.rewind_head(*head).await {
                        if let Some(previous) = previous_goal_mcp_connected {
                            self.goal_mcp_connected.store(previous, Ordering::Relaxed);
                        }
                        return Err(error.into());
                    }
                    // Rewind is durable now; always commit the fresh engine, even if the
                    // additional binding record cannot be written.
                    if let Err(error) = recorder
                        .record_backend_bound(
                            serde_json::json!({
                                "agent": self.agent_name,
                                "acp_session_id": new_session_id.to_string(),
                            })
                            .to_string(),
                        )
                        .await
                    {
                        warn!(%error, "Failed to record backend binding after rewind");
                    }
                    *self.pending_compact_summary.lock().await =
                        Some(crate::compact::checkpoint(context, None));
                }
                if let Some(server) = pending_nori_client_server {
                    server.commit(&self.goal_mcp_http_server).await;
                }
                debug!("Swapped active ACP session: {:?}", new_session_id);
                // An extension-driven goal belongs to the previous ACP
                // session; its native loop cannot follow the swap. Stop
                // mirroring (and best-effort clear the old loop, while
                // `session_id` still names the old session) so the
                // nori-client loop can take the local goal back over.
                self.stop_stale_ext_goal().await;
                let replaced = {
                    let mut active_session = self.session_id.write().await;
                    self.native_children.shutdown().await;
                    std::mem::replace(&mut *active_session, new_session_id.clone())
                };
                // Nothing the replaced session sends reaches the new one's ledger.
                self.retire_session(&replaced).await;
                self.orchestrator.bind(orchestrator::Binding {
                    session_id: new_session_id.clone(),
                    approval_policy: self.approval_policy_tx.subscribe(),
                });
                if matches!(mode, SessionSwapMode::ForkFromHead { .. }) {
                    self.fork_transcript(&new_session_id).await;
                }
                if !matches!(mode, SessionSwapMode::Rewind { .. })
                    && let Some(recorder) = self.transcript_recorder.read().await.clone()
                    && let Err(error) = recorder
                        .record_backend_bound(
                            serde_json::json!({
                                "agent": self.agent_name,
                                "acp_session_id": new_session_id.to_string(),
                            })
                            .to_string(),
                        )
                        .await
                {
                    warn!(%error, "Failed to record backend binding after session swap");
                }
                self.forward_client_event(ClientEvent::SessionCapabilitiesChanged(
                    nori_client_mcp::capabilities_update_for_nori_client(
                        &self.connection,
                        nori_client_advertised,
                        self.goal_mcp_connected.load(Ordering::Relaxed),
                        self.goal_mcp_http_server
                            .lock()
                            .await
                            .as_ref()
                            .is_some_and(nori_client_mcp::NoriClient::has_goal_tools),
                    ),
                ))
                .await;
                // A new session of the conversation is the moment to go back to the route the
                // user chose, when a fallback left it and the policy says `next_session`.
                if matches!(
                    mode,
                    SessionSwapMode::NewAfterCompact | SessionSwapMode::Rewind { .. }
                ) && let Some(request) = self.restore_primary(RestoreAt::SessionStart).await
                {
                    self.request_handoff(request);
                }
                Ok(new_session_id)
            }
            Err(err) => {
                if let Some(previous) = previous_goal_mcp_connected {
                    self.goal_mcp_connected.store(previous, Ordering::Relaxed);
                }
                Err(err)
            }
        }
    }

    /// Fork the active transcript into a fresh conversation seeded from the
    /// parent, freezing the parent on disk and swapping the active recorder +
    /// conversation id to the child.
    ///
    /// Best-effort: transcript persistence is non-fatal, so any failure is
    /// logged and the ACP session swap still stands. Emits the public
    /// `SessionForked` event after the swap so the event and all subsequent
    /// entries record into the child transcript.
    async fn fork_transcript(&self, new_acp_session_id: &acp::SessionId) {
        let Some(parent_recorder) = self.transcript_recorder.read().await.clone() else {
            return;
        };
        let previous_conversation_id = parent_recorder.session_id().to_string();

        // Freeze the parent before seeding from its file on disk.
        if let Err(error) = parent_recorder.flush().await {
            warn!("Failed to flush parent transcript before fork: {error}");
            return;
        }
        let seed_entries =
            match crate::transcript::read_seed_entries(parent_recorder.transcript_path()).await {
                Ok(entries) => entries,
                Err(error) => {
                    warn!("Failed to read parent transcript for fork: {error}");
                    return;
                }
            };

        let forked = match TranscriptRecorder::new_forked(
            &self.nori_home,
            &self.cwd,
            Some(self.agent_name.clone()),
            &self.cli_version,
            new_acp_session_id.to_string(),
            previous_conversation_id.clone(),
            seed_entries,
        )
        .await
        {
            Ok(recorder) => recorder,
            Err(error) => {
                warn!("Failed to create forked transcript: {error}");
                return;
            }
        };
        let new_conversation_id = forked.session_id().to_string();

        // Swap the active recorder + conversation id BEFORE emitting the event
        // so the event and every later entry record into the child.
        *self.transcript_recorder.write().await = Some(Arc::new(forked));
        if let Ok(conversation_id) = ConversationId::from_string(&new_conversation_id) {
            *self.conversation_id.write().await = conversation_id;
        }
        self.backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionForked(nori_protocol::SessionForked {
                    previous_conversation_id,
                    new_conversation_id,
                    new_acp_session_id: new_acp_session_id.clone(),
                }),
            )))
            .await
            .ok();
    }

    /// Branch the conversation at its current head via ACP `session/fork`,
    /// swapping the active session to the forked id.
    ///
    /// Gated on the agent advertising the fork capability and on the session
    /// being idle.
    pub(crate) async fn branch(&self) -> Result<()> {
        if self
            .connection
            .capabilities()
            .session_capabilities
            .fork
            .is_none()
        {
            anyhow::bail!("This agent does not support branching");
        }

        let from = self.session_id.read().await.clone();
        self.while_fenced("branch", async {
            self.swap_active_session(SessionSwapMode::ForkFromHead { from })
                .await
        })
        .await?;
        Ok(())
    }
}
