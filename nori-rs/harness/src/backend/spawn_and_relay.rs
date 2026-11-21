use super::*;

async fn forward_public_acp_event(
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    event: nori_protocol::AcpEvent,
) -> Result<()> {
    backend_event_tx
        .send(BackendEvent::Public(SessionEvent::Acp(event)))
        .await
        .map_err(|_| anyhow::anyhow!("session event receiver closed during bootstrap"))
}

async fn forward_public_session_events(
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    events: impl IntoIterator<Item = SessionEvent>,
) -> Result<()> {
    for event in events {
        backend_event_tx
            .send(BackendEvent::Public(event))
            .await
            .map_err(|_| anyhow::anyhow!("session event receiver closed during bootstrap"))?;
    }
    Ok(())
}

fn user_message_chunk(event: &nori_protocol::AcpEvent) -> Option<&acp::ContentChunk> {
    match event {
        nori_protocol::AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            notification,
        )) => match &notification.update {
            acp::SessionUpdate::UserMessageChunk(chunk) => Some(chunk),
            _ => None,
        },
        _ => None,
    }
}

/// Force the persisted default model into the agent's spawn configuration via
/// its out-of-band channel (see `ModelInjection`). Best-effort: a failure is
/// logged and never blocks startup. Preparation calls this before spawning the
/// subprocess so new, loaded, and resumed sessions all use the model injected
/// into the exact connection they later activate.
pub(super) fn inject_default_model(
    agent_config: &mut crate::registry::AcpAgentConfig,
    default_model: Option<&str>,
) {
    if let Some(default_model) = default_model
        && let Err(err) = agent_config.inject_model(default_model)
    {
        warn!(%err, "failed to inject default model into agent spawn config");
    }
}

/// What the agent announced about the session during setup, for the ledger to start with.
type SetupFacts = Vec<acp::SessionUpdate>;

async fn forward_setup_event(
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    event: crate::connection::ConnectionEvent,
    agent: &str,
    facts: &mut SetupFacts,
) -> Result<bool> {
    match event {
        crate::connection::ConnectionEvent::Acp(event) => {
            let is_response = matches!(&*event, nori_protocol::AcpEvent::Response { .. });
            forward_public_acp_event(backend_event_tx, *event).await?;
            Ok(is_response)
        }
        // The public ACP notification immediately precedes this private reducer projection.
        // Reducer processing starts after setup, knowing the session facts announced here.
        crate::connection::ConnectionEvent::SessionUpdate(update) => {
            facts.push(update);
            Ok(false)
        }
        crate::connection::ConnectionEvent::Subagent(wire) => {
            forward_public_session_events(
                backend_event_tx,
                [SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    subagent_identity::native_child(wire, agent),
                ))],
            )
            .await?;
            Ok(false)
        }
        crate::connection::ConnectionEvent::Barrier(reply) => {
            let _ = reply.send(());
            Ok(false)
        }
        crate::connection::ConnectionEvent::DelegatedRequest(request) => {
            let _ = request
                .response_tx
                .send(Ok(acp::ClientResponse::RequestPermissionResponse(
                    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
                )));
            Ok(false)
        }
        crate::connection::ConnectionEvent::SessionClosed => {
            anyhow::bail!("ACP session closed during bootstrap")
        }
        crate::connection::ConnectionEvent::ChildExited {
            status,
            stderr_tail,
        } => {
            anyhow::bail!("ACP agent exited during bootstrap (status: {status:?}): {stderr_tail}")
        }
    }
}

async fn forward_setup_events_through_response(
    event_rx: &mut mpsc::Receiver<crate::connection::ConnectionEvent>,
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    agent: &str,
    facts: &mut SetupFacts,
) -> Result<()> {
    loop {
        let event = event_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("ACP bootstrap event stream closed"))?;
        if forward_setup_event(backend_event_tx, event, agent, facts).await? {
            return Ok(());
        }
    }
}

async fn forward_available_setup_events(
    event_rx: &mut mpsc::Receiver<crate::connection::ConnectionEvent>,
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    agent: &str,
    facts: &mut SetupFacts,
) -> Result<()> {
    while let Ok(event) = event_rx.try_recv() {
        forward_setup_event(backend_event_tx, event, agent, facts).await?;
    }
    Ok(())
}

impl AcpBackend {
    pub(super) async fn wait_for_prompt_phase(&self) {
        let mut gate = self.prompt_phase_gate.lock().await.clone();
        let Some(gate) = gate.as_mut() else {
            return;
        };
        if !*gate.borrow() {
            let _ = gate.changed().await;
        }
    }

    /// Activate a prepared ACP connection with a new session.
    ///
    /// This will:
    /// 1. Consume the already initialized connection
    /// 2. Create a session on that connection
    /// 3. Send a synthetic `SessionConfigured` event
    /// 4. Start background tasks for control-plane forwarding, approvals, and normalized session updates
    ///
    /// # Arguments
    /// * `prepared` - The uniquely owned initialized ACP connection
    /// * `backend_event_tx` - Channel to send ACP backend events to the TUI
    ///
    /// # Returns
    /// A connected `AcpBackend` ready to receive operations.
    pub async fn spawn(
        prepared: super::prepared::PreparedAgent,
        backend_event_tx: mpsc::Sender<BackendEvent>,
    ) -> Result<Self> {
        let super::prepared::PreparedAgentParts {
            connection,
            mut event_rx,
            config,
            setup_events,
        } = prepared.into_parts();
        let config = &config;
        let cwd = config.cwd.clone();
        let (orchestrator, orchestrator_signals) =
            orchestrator::Orchestrator::new(config.delegation.clone(), backend_event_tx.clone());

        let agent_config = get_agent_config(&config.agent)?;
        debug!(
            "Activating prepared ACP backend for agent: {}",
            config.agent
        );
        forward_public_session_events(&backend_event_tx, setup_events).await?;

        let thread_goal_state = Arc::new(Mutex::new(thread_goal::ThreadGoalState::default()));
        let goal_mcp_connected = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let goal_mcp_http_server = Arc::new(Mutex::new(None));
        let policy_before_session =
            super::host_tools::accepted_by(&connection) || nori_client_mcp::reachable(&connection);
        let (workflow, machine_snapshot) = if policy_before_session {
            workflow::prepare_workflow(
                &config.nori_home,
                &cwd,
                orchestrator.project_trusted(),
                orchestrator.is_child(),
                None,
            )
            .await?
        } else {
            (Arc::default(), Arc::new(tokio::sync::OnceCell::new()))
        };

        let mut mcp_servers = crate::connection::mcp::to_acp_mcp_servers(
            &config.mcp_servers,
            config.mcp_oauth_credentials_store_mode,
        );
        nudox::add_to(&mut mcp_servers, &cwd, &config.mcp_servers, config.nudox).await;
        let nori_client_server = nori_client_mcp::register_for_session(
            &connection,
            &mut mcp_servers,
            Arc::clone(&thread_goal_state),
            backend_event_tx.clone(),
            Arc::clone(&goal_mcp_connected),
            orchestrator.clone(),
            nori_client_mcp::Workspace {
                cwd: cwd.clone(),
                workflow: Arc::clone(&workflow),
            },
        )
        .await?;
        if let Some(server) = nori_client_server {
            server.commit(&goal_mcp_http_server).await;
        }
        // Counting the history file does not depend on the session, so it
        // runs while the agent creates one.
        let history_task = tokio::spawn({
            let nori_home = config.nori_home.clone();
            async move { crate::message_history::history_metadata(&nori_home).await }
        });
        // The user's session goes first; other agents warm up behind it.
        let prewarm_servers = (!config.delegation.is_child()).then(|| mcp_servers.clone());
        let session_result = connection.create_session(&cwd, mcp_servers).await;
        if let Some(servers) = prewarm_servers {
            prewarm::others(config.agent.clone(), cwd.clone(), &servers);
        }
        let mut setup_facts = SetupFacts::new();
        let setup_forward_result = forward_setup_events_through_response(
            &mut event_rx,
            &backend_event_tx,
            &config.agent,
            &mut setup_facts,
        )
        .await;
        let session_id = match session_result {
            Ok(id) => {
                setup_forward_result?;
                id
            }
            Err(error) => {
                if let Err(forward_error) = setup_forward_result {
                    warn!(%forward_error, "failed to forward ACP new-session error response");
                }
                return Err(enhance_agent_error(error, &agent_config));
            }
        };

        debug!("ACP session created: {:?}", session_id);

        // Apply default model from config if one is set for this agent
        if let Some(ref default_model) = config.default_model {
            session_defaults::apply_default_model(&connection, &session_id, default_model).await;
        }
        forward_available_setup_events(
            &mut event_rx,
            &backend_event_tx,
            &config.agent,
            &mut setup_facts,
        )
        .await?;

        let capabilities_update = nori_client_mcp::capabilities_update_for_session(
            &connection,
            &goal_mcp_connected,
            goal_mcp_http_server
                .lock()
                .await
                .as_ref()
                .is_some_and(nori_client_mcp::NoriClient::has_goal_tools),
        );

        let connection = Arc::new(connection);
        let pending_approvals = Arc::new(Mutex::new(Vec::new()));
        let mut session_driver = session_runtime_driver::SessionDriver::new();
        let policy = workflow.lock().await;
        session_driver.set_policy(policy.loops, policy.compaction.clone());
        drop(policy);
        session_driver.absorb_setup(&setup_facts);
        let session_driver = Arc::new(Mutex::new(session_driver));
        let (session_event_tx, session_event_rx) = mpsc::channel(128);
        let (prompt_result_tx, prompt_result_rx) = mpsc::channel(128);
        let use_native_notifications =
            config.os_notifications == crate::config::OsNotifications::Enabled;
        let user_notifier = Arc::new(crate::UserNotifier::new(
            config.notify.clone(),
            use_native_notifications,
        ));

        let idle_timer_abort = Arc::new(Mutex::new(None));

        // Create watch channel for dynamic approval policy updates
        let (approval_policy_tx, approval_policy_rx) = watch::channel(config.approval_policy);

        // Get history metadata
        let (history_log_id, history_entry_count) = history_task.await.unwrap_or_default();

        // Initialize a canonical transcript for every session, including delegated children.
        // The parent's separate transcript retains relayed child events for its own replay.
        let transcript_recorder = if !config.delegation.record_transcript {
            None
        } else if let Some(recorder) = &config.handoff {
            Some(Arc::clone(recorder))
        } else {
            Some(Arc::new(
                TranscriptRecorder::new_for(
                    &config.nori_home,
                    &cwd,
                    Some(config.agent.clone()),
                    &config.cli_version,
                    Some(session_id.to_string()),
                    config.delegation.is_child(),
                )
                .await
                .map_err(|error| {
                    anyhow::anyhow!("failed to initialize canonical transcript: {error}")
                })?,
            ))
        };
        if policy_before_session {
            workflow::record_prepared_snapshot(transcript_recorder.as_ref(), &machine_snapshot)
                .await;
        }
        let transcript_id = transcript_recorder
            .as_ref()
            .map(|recorder| recorder.session_id().to_string());
        let transcript_path = transcript_recorder
            .as_ref()
            .map(|recorder| recorder.transcript_path().to_path_buf());
        let conversation_id = transcript_recorder
            .as_ref()
            .and_then(|recorder| ConversationId::from_string(recorder.session_id()).ok())
            .unwrap_or_default();
        if config.handoff.is_none()
            && let Some(recorder) = &transcript_recorder
            && let Err(error) = recorder
                .record_backend_bound(
                    serde_json::json!({
                        "agent": config.agent,
                        "acp_session_id": session_id.to_string(),
                    })
                    .to_string(),
                )
                .await
        {
            warn!(%error, "Failed to record initial backend binding");
        }
        let pending_static_context = orchestrator
            .initial_catalog_context(
                session_context_for_connection(config, &connection),
                &workflow,
                goal_mcp_http_server.lock().await.is_some(),
            )
            .await;
        let goal_ext_capability =
            goal_ext::GoalExtCapability::from_initialize_meta(connection.initialize_meta());
        let transcript_recorder_cell = Arc::new(RwLock::new(transcript_recorder));
        let native_children = Arc::new(super::native_child_sessions::NativeChildren::default());
        if transcript_recorder_cell
            .read()
            .await
            .as_ref()
            .is_some_and(|recorder| recorder.has_conversation_log())
        {
            native_children
                .configure_history(Arc::new(super::native_child_history::NativeHistory::new(
                    config.nori_home.clone(),
                    config.cwd.clone(),
                    config.agent.clone(),
                    config.cli_version.clone(),
                    transcript_recorder_cell.clone(),
                )))
                .await;
        }
        let backend = Self {
            connection,
            session_id: Arc::new(RwLock::new(session_id)),
            backend_event_tx: backend_event_tx.clone(),
            cwd: cwd.clone(),
            pending_approvals: Arc::clone(&pending_approvals),
            withdrawn_questions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            pending_prompt_submissions: Arc::new(Mutex::new(HashMap::new())),
            pending_task_results: Arc::new(Mutex::new(HashMap::new())),
            user_notifier: Arc::clone(&user_notifier),
            idle_timer_abort: Arc::clone(&idle_timer_abort),
            nori_home: config.nori_home.clone(),
            history_persistence: config.history_persistence,
            acp_proxy: config.acp_proxy.clone(),
            conversation_id: Arc::new(RwLock::new(conversation_id)),
            approval_policy_tx,
            compact_nudged: Arc::new(AtomicBool::new(false)),
            auto_compaction: Arc::default(),
            pending_compact_summary: Arc::new(Mutex::new(
                config
                    .initial_context
                    .as_deref()
                    .map(|context| crate::compact::checkpoint(context, None)),
            )),
            thread_goal_state,
            goal_mcp_connected,
            goal_mcp_http_server,
            goal_ext_capability,
            pending_hook_context: Arc::new(Mutex::new(None)),
            pending_static_context: Arc::new(Mutex::new(pending_static_context)),
            transcript_recorder: transcript_recorder_cell,
            session_event_tx: session_event_tx.clone(),
            prompt_result_tx: prompt_result_tx.clone(),
            prompt_phase_gate: Arc::new(Mutex::new(None)),
            notify_after_idle: config.notify_after_idle,
            edit_journal: Arc::new(EditJournal::for_cwd(
                &cwd,
                &config.nori_home,
                backend_event_tx.clone(),
            )),
            is_first_prompt: Arc::new(Mutex::new(true)),
            workflow,
            machine_snapshot,
            workflow_ready: tokio::sync::watch::channel(
                policy_before_session || config.delegation.is_child(),
            )
            .0,
            program_effects: Arc::new(tokio::sync::RwLock::new(())),
            prompt_summary_enabled: config.prompt_summary_enabled,
            agent_name: config.agent.clone(),
            handoff_candidate: config.handoff.is_some(),
            cli_version: config.cli_version.clone(),
            auto_worktree: config.auto_worktree,
            auto_worktree_repo_root: config.auto_worktree_repo_root.clone(),
            session_end_hooks: config.session_end_hooks.clone(),
            pre_user_prompt_hooks: config.pre_user_prompt_hooks.clone(),
            post_user_prompt_hooks: config.post_user_prompt_hooks.clone(),
            post_agent_response_hooks: config.post_agent_response_hooks.clone(),
            async_session_end_hooks: config.async_session_end_hooks.clone(),
            async_pre_user_prompt_hooks: config.async_pre_user_prompt_hooks.clone(),
            async_post_user_prompt_hooks: config.async_post_user_prompt_hooks.clone(),
            async_post_agent_response_hooks: config.async_post_agent_response_hooks.clone(),
            script_timeout: config.script_timeout,
            session_driver: Arc::clone(&session_driver),
            mcp_servers: config.mcp_servers.clone(),
            nudox: config.nudox,
            mcp_oauth_credentials_store_mode: config.mcp_oauth_credentials_store_mode,
            is_shutting_down: Arc::new(AtomicBool::new(false)),
            prompt_task_abort: Arc::new(Mutex::new(None)),
            retired_sessions: Arc::default(),
            recovery: super::route_moves::SessionRecovery::new(config.session_context.clone()),
            cancel_timeout_abort: Arc::new(Mutex::new(None)),
            runtime_task_abort: Arc::new(Mutex::new(None)),
            relay_task_abort: Arc::new(Mutex::new(None)),
            orchestrator,
            native_children,
            user_bash_session: Arc::new(Mutex::new(None)),
        };

        if !policy_before_session && !backend.orchestrator.is_child() {
            backend.load_workflow_without_tools();
        }

        *backend.runtime_task_abort.lock().await =
            Some(backend.spawn_runtime_actor(session_event_rx));
        backend.orchestrator.bind(orchestrator::Binding {
            session_id: backend.session_id().await,
            approval_policy: backend.approval_policy_tx.subscribe(),
        });
        backend.serve_orchestrator(orchestrator_signals);
        orchestrator::remember_models(&config.nori_home, &config.agent, &backend.config_options());

        // Execute session_start hooks
        run_session_start_hooks(
            &config.session_start_hooks,
            config.script_timeout,
            &backend_event_tx,
            Some(&backend.pending_hook_context),
        )
        .await;

        // Fire-and-forget async session start hooks
        let _ = crate::hooks::execute_hooks_fire_and_forget(
            config.async_session_start_hooks.clone(),
            config.script_timeout,
            HashMap::new(),
        );

        backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::CapabilitiesChanged(public_nori_capabilities(
                    capabilities_update,
                )),
            )))
            .await
            .ok();
        backend_event_tx
            .send(BackendEvent::Public(nori_protocol::SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionStarted(nori_protocol::SessionStarted {
                    transcript_id,
                    acp_session_id: backend.session_id().await,
                    cwd: cwd.clone(),
                    transcript_path,
                    history_log_id: i64::try_from(history_log_id).unwrap_or(i64::MAX),
                    history_entry_count: i64::try_from(history_entry_count).unwrap_or(i64::MAX),
                    config_options: backend.config_options(),
                    swap: None,
                }),
            )))
            .await
            .ok();

        backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionReady(backend.session_id().await),
            )))
            .await
            .ok();

        // Spawn reducer loop: processes all transport events and prompt
        // results through the serialized reducer/runtime pipeline.
        let relay_task = tokio::spawn(Self::run_connection_event_relay(
            backend.clone(),
            event_rx,
            prompt_result_rx,
            approval_policy_rx,
        ));
        *backend.relay_task_abort.lock().await = Some(relay_task.abort_handle());

        Ok(backend)
    }

    /// Background task that processes transport events and prompt results
    /// through the serialized session reducer/runtime pipeline.
    pub(super) async fn run_connection_event_relay(
        backend: AcpBackend,
        mut event_rx: mpsc::Receiver<crate::connection::ConnectionEvent>,
        mut prompt_result_rx: mpsc::Receiver<session_reducer::InboundEvent>,
        approval_policy_rx: watch::Receiver<AskForApproval>,
    ) {
        let approval_policy_rx = approval_policy_rx;
        let mut relay_seq = 0_i64;
        let mut suppress_next_session_update = false;
        let mut child_edits = super::child_edits::ChildEdits::default();
        // usage: samples of what the agent reports about its own consumption.
        let mut usage_tap = crate::usage::UsageTap::new(&backend.nori_home, backend.agent_name())
            .for_actor(
                backend
                    .orchestrator
                    .seed()
                    .actor_view
                    .as_ref()
                    .map(ToString::to_string),
            );
        loop {
            tokio::select! {
                biased;
                maybe_event = event_rx.recv() => {
                    match maybe_event {
                        Some(crate::connection::ConnectionEvent::SessionUpdate(update)) => {
                            if suppress_next_session_update {
                                suppress_next_session_update = false;
                                debug!(
                                    target: "acp_event_flow",
                                    "Suppressed downstream echo of the active prompt"
                                );
                                continue;
                            }
                            backend.wait_for_prompt_phase().await;
                            backend.observe_session_update_for_goal_ext(&update).await;
                            // usage: the root session's usage and session-info updates.
                            usage_tap.observe_root(&backend.session_id().await.to_string(), &update);
                            relay_seq += 1;
                            // One per streamed chunk: trace, so a long reply is not a long log.
                            tracing::trace!(
                                target: "acp_event_flow",
                                relay_seq,
                                relay_source = "transport_event_rx",
                                update_kind = crate::connection::session_update_kind(&update),
                                "Relaying session/update into serialized session runtime"
                            );
                            let _ = backend
                                .session_event_tx
                                .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                                    session_reducer::InboundEvent::Notification(Box::new(update)),
                                ))
                                .await;
                        }
                        Some(crate::connection::ConnectionEvent::Subagent(wire)) => {
                            if !backend.native_children.observe(&backend.connection, &backend.backend_event_tx, &*backend.session_id.read().await, &wire).await { continue; }
                            backend.wait_for_prompt_phase().await;
                            let _ = backend
                                .backend_event_tx
                                .send(BackendEvent::Public(SessionEvent::Nori(
                                    nori_protocol::NoriEvent::Subagent(subagent_identity::native_child(wire, backend.agent_name())),
                                )))
                                .await;
                        }
                        Some(crate::connection::ConnectionEvent::Acp(event)) => {
                            if super::user_question::is_question_cancellation(&event) {
                                let _ = backend.session_event_tx.send(session_runtime_driver::SessionRuntimeInput::QuestionCancelled(event)).await;
                                continue;
                            }
                            // Traffic of a session a swap replaced never reaches the ledger, nor does
                            // the reducer input that follows its notification.
                            if backend.drops_retired(&event) {
                                suppress_next_session_update = true;
                                continue;
                            }
                            backend.wait_for_prompt_phase().await;
                            if let nori_protocol::AcpEvent::ChildNotification(notification) = &*event {
                                if !backend.native_children.observe_notification(notification).await { continue; }
                                child_edits.observe(notification, &backend.edit_journal, &backend.cwd);
                                // usage: a subagent's usage and session-info updates.
                                usage_tap.observe_child(&notification.session_id.to_string(), &notification.update);
                            }
                            suppress_next_session_update = if let Some(chunk) =
                                user_message_chunk(&event)
                            {
                                backend
                                    .session_driver
                                    .lock()
                                    .await
                                    .owns_active_prompt_echo(chunk)
                            } else {
                                false
                            };
                            // A prompt's response ends its turn where it stands on the wire: what
                            // the agent sends after it (a permission request, more work) is
                            // after the turn, not in it. The task that awaited the response
                            // reports the same outcome later; the ledger takes the first.
                            let answered = match &*event {
                                nori_protocol::AcpEvent::Response {
                                    request_id,
                                    response: Ok(nori_protocol::acp::v1::AgentResponse::PromptResponse(response)),
                                } => Some((request_id.clone(), response.stop_reason)),
                                _ => None,
                            };
                            if !suppress_next_session_update {
                                let _ = backend
                                    .backend_event_tx
                                    .send(BackendEvent::Public(nori_protocol::SessionEvent::Acp(
                                        *event,
                                    )))
                                    .await;
                            }
                            if let Some((request_id, stop_reason)) = answered {
                                let _ = backend
                                    .session_event_tx
                                    .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                                        session_reducer::InboundEvent::PromptResultFor {
                                            request_id,
                                            result: super::prompt_lifecycle::PromptResult::Response(stop_reason),
                                        },
                                    ))
                                    .await;
                            }
                        }
                        Some(crate::connection::ConnectionEvent::SessionClosed) => {
                            backend.withdraw_all_questions().await;
                            backend.connection_lost().await;
                            let _ = backend
                                .backend_event_tx
                                .send(BackendEvent::Public(SessionEvent::Nori(
                                    nori_protocol::NoriEvent::SessionEnded(
                                        nori_protocol::SessionEnded {
                                            reason: nori_protocol::SessionEndReason::Closed,
                                            message: None,
                                        },
                                    ),
                                )))
                                .await;
                            break;
                        }
                        Some(crate::connection::ConnectionEvent::Barrier(reply)) => { let _ = reply.send(()); }
                        Some(crate::connection::ConnectionEvent::DelegatedRequest(request)) => {
                            let Some(request) = backend.decline_retired_request(request).await else {
                                continue;
                            };
                            backend.wait_for_prompt_phase().await;
                            relay_seq += 1;
                            let current_policy = *approval_policy_rx.borrow();
                            debug!(
                                target: "acp_event_flow",
                                relay_seq,
                                relay_source = "transport_event_rx",
                                request_id = %request.request_id,
                                "Relaying permission request into serialized session runtime"
                            );
                            let _ = backend
                                .session_event_tx
                                .send(
                                    session_runtime_driver::SessionRuntimeInput::PermissionRequest {
                                        pending_request: Box::new(PendingApprovalRequest {
                                            request,
                                        }),
                                        current_policy,
                                    },
                                )
                                .await;
                        }
                        Some(crate::connection::ConnectionEvent::ChildExited {
                            status,
                            stderr_tail,
                        }) => {
                            backend.withdraw_all_questions().await;
                            backend.wait_for_prompt_phase().await;
                            if !backend.is_shutting_down.load(Ordering::Relaxed) {
                                let mut message = match status {
                                    Some(code) => format!(
                                        "Agent process exited unexpectedly (exit code {code})."
                                    ),
                                    None => {
                                        "Agent process exited unexpectedly.".to_string()
                                    }
                                };
                                if !stderr_tail.is_empty() {
                                    message.push('\n');
                                    message.push_str(&stderr_tail);
                                }
                                let request_id = backend
                                    .session_driver
                                    .lock()
                                    .await
                                    .active_wire_request_id();
                                let _ = backend
                                    .backend_event_tx
                                    .send(BackendEvent::Public(SessionEvent::Nori(
                                        nori_protocol::NoriEvent::RequestFailed(
                                            nori_protocol::RequestFailure {
                                                request_id,
                                                message: message.clone(),
                                                kind: nori_protocol::RequestFailureKind::Fatal,
                                            },
                                        ),
                                    )))
                                    .await;
                                // Fail any in-flight prompt loudly instead of
                                // letting it wait on a dead transport forever.
                                if let Some(task) = backend.prompt_task_abort.lock().await.take() {
                                    task.abort();
                                }
                                // Whatever turn was open ends as failed and everything the agent
                                // waited on is answered as cancelled.
                                backend.connection_lost().await;
                                // The session ends last: nothing after it reaches the client, so
                                // the turn's closing (idle phase, closed tool calls) comes first.
                                let _ = backend
                                    .backend_event_tx
                                    .send(BackendEvent::Public(SessionEvent::Nori(
                                        nori_protocol::NoriEvent::SessionEnded(
                                            nori_protocol::SessionEnded {
                                                reason: nori_protocol::SessionEndReason::ConnectionLost,
                                                message: Some(message),
                                            },
                                        ),
                                    )))
                                    .await;
                            }
                            break;
                        }
                        None => break,
                    }
                }
                maybe_result = prompt_result_rx.recv() => {
                    match maybe_result {
                        Some(result) => {
                            relay_seq += 1;
                            debug!(
                                target: "acp_event_flow",
                                relay_seq,
                                relay_source = "prompt_result_rx",
                                inbound_event = session_reducer::inbound_event_kind(&result),
                                "Relaying prompt result into serialized session runtime"
                            );
                            let _ = backend
                                .session_event_tx
                                .send(session_runtime_driver::SessionRuntimeInput::Reducer(result))
                                .await;
                        }
                        None => {
                            while let Some(event) = event_rx.recv().await {
                                match event {
                                    crate::connection::ConnectionEvent::Subagent(wire) => {
                                        if !backend.native_children.observe(&backend.connection, &backend.backend_event_tx, &*backend.session_id.read().await, &wire).await { continue; }
                                        let _ = backend
                                            .backend_event_tx
                                            .send(BackendEvent::Public(SessionEvent::Nori(
                                                nori_protocol::NoriEvent::Subagent(subagent_identity::native_child(wire, backend.agent_name())),
                                            )))
                                            .await;
                                    }
                                    crate::connection::ConnectionEvent::Acp(event) => {
                                        if super::user_question::is_question_cancellation(&event) {
                                let _ = backend.session_event_tx.send(session_runtime_driver::SessionRuntimeInput::QuestionCancelled(event)).await;
                                continue;
                            }
                                        if let nori_protocol::AcpEvent::ChildNotification(notification) = &*event
                                            && !backend.native_children.observe_notification(notification).await { continue; }
                                        suppress_next_session_update = if let Some(chunk) =
                                            user_message_chunk(&event)
                                        {
                                            backend
                                                .session_driver
                                                .lock()
                                                .await
                                                .owns_active_prompt_echo(chunk)
                                        } else {
                                            false
                                        };
                                        if !suppress_next_session_update {
                                            let _ = backend
                                                .backend_event_tx
                                                .send(BackendEvent::Public(
                                                    nori_protocol::SessionEvent::Acp(*event),
                                                ))
                                                .await;
                                        }
                                    }
                                    crate::connection::ConnectionEvent::SessionUpdate(update) => {
                                        if suppress_next_session_update {
                                            suppress_next_session_update = false;
                                            continue;
                                        }
                                        backend.observe_session_update_for_goal_ext(&update).await;
                                        relay_seq += 1;
                                        debug!(
                                            target: "acp_event_flow",
                                            relay_seq,
                                            relay_source = "transport_event_rx_drain",
                                            update_kind = crate::connection::session_update_kind(&update),
                                            "Draining session/update after prompt result channel closed"
                                        );
                                        let _ = backend
                                            .session_event_tx
                                            .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                                                session_reducer::InboundEvent::Notification(Box::new(update)),
                                            ))
                                            .await;
                                    }
                                    crate::connection::ConnectionEvent::SessionClosed => {
                                        backend.withdraw_all_questions().await;
                                        let _ = backend
                                            .backend_event_tx
                                            .send(BackendEvent::Public(SessionEvent::Nori(
                                                nori_protocol::NoriEvent::SessionEnded(
                                                    nori_protocol::SessionEnded {
                                                        reason: nori_protocol::SessionEndReason::Closed,
                                                        message: None,
                                                    },
                                                ),
                                            )))
                                            .await;
                                        break;
                                    }
                                    // This drain only runs during teardown
                                    // (prompt channel closed); the exit was
                                    // either user-initiated or already
                                    // reported by the main relay arm.
                                    crate::connection::ConnectionEvent::ChildExited { .. } => { backend.withdraw_all_questions().await; }
                                    crate::connection::ConnectionEvent::Barrier(reply) => { let _ = reply.send(()); }
                                    crate::connection::ConnectionEvent::DelegatedRequest(request) => {
                                        relay_seq += 1;
                                        let current_policy = *approval_policy_rx.borrow();
                                        debug!(
                                            target: "acp_event_flow",
                                            relay_seq,
                                            relay_source = "transport_event_rx_drain",
                                            request_id = %request.request_id,
                                            "Draining permission request after prompt result channel closed"
                                        );
                                        let _ = backend
                                            .session_event_tx
                                            .send(
                                                session_runtime_driver::SessionRuntimeInput::PermissionRequest {
                                                    pending_request: Box::new(PendingApprovalRequest {
                                                        request,
                                                    }),
                                                    current_policy,
                                                },
                                            )
                                            .await;
                                    }
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
        if let Some(abort) = backend.runtime_task_abort.lock().await.take() {
            abort.abort();
        }
    }
}
