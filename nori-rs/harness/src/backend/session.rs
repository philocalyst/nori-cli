use super::*;

/// Build the session's MCP server list (CLI-configured servers plus the
/// nori-client goal server when the agent advertises HTTP MCP) and commit
/// the registered server. Shared by every branch of `resume_session`.
#[expect(
    clippy::too_many_arguments,
    reason = "each is a piece of the backend that the session's tools share"
)]
async fn session_mcp_servers(
    config: &AcpBackendConfig,
    connection: &AcpConnection,
    thread_goal_state: &Arc<Mutex<thread_goal::ThreadGoalState>>,
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    goal_mcp_connected: &Arc<std::sync::atomic::AtomicBool>,
    goal_mcp_http_server: &Arc<Mutex<Option<nori_client_mcp::NoriClient>>>,
    orchestrator: &orchestrator::Orchestrator,
    workflow: &Arc<Mutex<workflow::WorkflowState>>,
) -> Result<Vec<acp::McpServer>> {
    let mut mcp_servers = crate::connection::mcp::to_acp_mcp_servers(
        &config.mcp_servers,
        config.mcp_oauth_credentials_store_mode,
    );
    let nori_client_server = nori_client_mcp::register_for_session(
        connection,
        &mut mcp_servers,
        Arc::clone(thread_goal_state),
        backend_event_tx.clone(),
        Arc::clone(goal_mcp_connected),
        orchestrator.clone(),
        nori_client_mcp::Workspace {
            cwd: config.cwd.clone(),
            workflow: Arc::clone(workflow),
        },
    )
    .await?;
    if let Some(server) = nori_client_server {
        server.commit(goal_mcp_http_server).await;
    }
    Ok(mcp_servers)
}

fn retarget_replay_event(
    mut event: nori_protocol::SessionEvent,
    session_id: &acp::SessionId,
) -> nori_protocol::SessionEvent {
    if let nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
        acp::AgentNotification::SessionNotification(notification),
    )) = &mut event
    {
        notification.session_id = session_id.clone();
    }
    event
}

struct ReplayBatch {
    source: nori_protocol::ReplaySource,
    events: Vec<nori_protocol::SessionEvent>,
}

impl ReplayBatch {
    fn new(
        source: nori_protocol::ReplaySource,
        events: Vec<nori_protocol::SessionEvent>,
    ) -> Option<Self> {
        if events.is_empty() {
            None
        } else {
            Some(Self { source, events })
        }
    }
}

fn drain_setup_session_events(
    event_rx: &mut mpsc::Receiver<crate::connection::ConnectionEvent>,
    agent: &str,
) -> Result<Vec<nori_protocol::SessionEvent>> {
    let mut events = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        match event {
            crate::connection::ConnectionEvent::Acp(event) => {
                events.push(nori_protocol::SessionEvent::Acp(*event));
            }
            // The exact notification is the preceding ACP event. The private
            // reducer projection starts after setup completes.
            crate::connection::ConnectionEvent::SessionUpdate(_) => {}
            crate::connection::ConnectionEvent::Subagent(wire) => {
                events.push(nori_protocol::SessionEvent::Nori(
                    nori_protocol::NoriEvent::Subagent(subagent_identity::native_child(
                        wire, agent,
                    )),
                ));
            }
            crate::connection::ConnectionEvent::Barrier(reply) => {
                let _ = reply.send(());
            }
            crate::connection::ConnectionEvent::DelegatedRequest(request) => {
                let _ =
                    request
                        .response_tx
                        .send(Ok(acp::ClientResponse::RequestPermissionResponse(
                            acp::RequestPermissionResponse::new(
                                acp::RequestPermissionOutcome::Cancelled,
                            ),
                        )));
            }
            crate::connection::ConnectionEvent::SessionClosed => {
                anyhow::bail!("ACP session closed during setup");
            }
            crate::connection::ConnectionEvent::ChildExited {
                status,
                stderr_tail,
            } => {
                anyhow::bail!("ACP agent exited during setup (status: {status:?}): {stderr_tail}");
            }
        }
    }
    Ok(events)
}

async fn forward_setup_session_events(
    backend_event_tx: &mpsc::Sender<BackendEvent>,
    events: impl IntoIterator<Item = nori_protocol::SessionEvent>,
) -> Result<()> {
    for event in events {
        backend_event_tx
            .send(BackendEvent::Public(event))
            .await
            .map_err(|_| anyhow::anyhow!("session event receiver closed during setup"))?;
    }
    Ok(())
}

fn response_request_id(events: &[nori_protocol::SessionEvent]) -> Option<acp::RequestId> {
    events.iter().find_map(|event| match event {
        nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Response {
            request_id, ..
        }) => Some(request_id.clone()),
        nori_protocol::SessionEvent::Acp(_) | nori_protocol::SessionEvent::Nori(_) => None,
    })
}

impl AcpBackend {
    /// Resume a previous ACP session.
    ///
    /// If the agent supports `session/load` (via capabilities) and an
    /// `acp_session_id` is provided, the existing server-side resume path is
    /// used. Otherwise a client-side replay fallback is used: a fresh session
    /// is created via `session/new`, normalized replay entries are derived from
    /// the transcript, and a summary is stored in `pending_compact_summary` so
    /// it gets prepended to the first prompt.
    pub async fn resume_session(
        prepared: super::prepared::PreparedAgent,
        acp_session_id: Option<&str>,
        transcript: Option<&crate::transcript::Transcript>,
        backend_event_tx: mpsc::Sender<BackendEvent>,
    ) -> Result<Self> {
        let super::prepared::PreparedAgentParts {
            connection,
            event_rx,
            config,
            setup_events,
        } = prepared.into_parts();
        let config = &config;
        let cwd = config.cwd.clone();
        let (orchestrator, orchestrator_signals) =
            orchestrator::Orchestrator::new(config.delegation.clone(), backend_event_tx.clone());

        debug!(
            "Resuming ACP session (acp_session_id={:?}) for agent: {}",
            acp_session_id, config.agent
        );

        let agent_config = get_agent_config(&config.agent)?;
        let initial_setup_events = setup_events.clone();
        forward_setup_session_events(&backend_event_tx, setup_events).await?;
        let mut event_rx = event_rx;

        let supports_load_session = connection.capabilities().load_session;
        let supports_session_resume = connection
            .capabilities()
            .session_capabilities
            .resume
            .is_some();
        let acp_session_id = acp_session_id
            .filter(|_| !transcript.is_some_and(|transcript| transcript.requires_fresh_binding));
        let initial_goal_replay_events = transcript
            .map(transcript_to_replay_client_events)
            .unwrap_or_default();
        let thread_goal_state = Arc::new(Mutex::new(
            thread_goal::ThreadGoalState::from_replay_events(&initial_goal_replay_events),
        ));
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
                transcript.map(|transcript| {
                    crate::transcript::TranscriptLoader::new(config.nori_home.clone())
                        .session_path(&transcript.meta.project_id, &transcript.meta.session_id)
                }),
            )
            .await?
        } else {
            (Arc::default(), Arc::new(tokio::sync::OnceCell::new()))
        };

        // Either load the session server-side or create a fresh session for
        // client-side replay.
        //
        // If server-side load_session fails at runtime and a local transcript
        // is available, fall back to client-side replay. Without a transcript,
        // propagate the failure: creating an empty replacement session would
        // abandon the session the user asked to reattach to.
        // The sixth tuple element carries buffered replay events from
        // server-side session/load.  We must NOT spawn a relay task for
        // these events until *after* resume_session has finished sending
        // its own events (SessionConfigured, Warning, etc.) to event_tx,
        // because the relay can fill the bounded channel and block
        // resume_session from sending.
        let (
            session_id,
            pending_summary,
            is_first_prompt_val,
            used_fallback,
            mut deferred_setup_session_events,
            deferred_replay_client_events,
            mut deferred_replay_batches,
            event_rx,
            session_driver_state,
            held_requests,
        ) = if let Some(sid) = acp_session_id
            .filter(|_| supports_load_session && !(supports_session_resume && transcript.is_some()))
        {
            debug!("Agent supports session/load — using server-side resume");

            // Collect replay events into a buffer. The collector runs until
            // load_session() finishes and signals completion via the oneshot.
            let (load_done_tx, load_done_rx) = tokio::sync::oneshot::channel::<()>();
            let (load_started_tx, load_started_rx) = tokio::sync::oneshot::channel();
            let agent = config.agent.clone();
            let collect_handle = tokio::spawn(async move {
                let mut event_rx = event_rx;
                let mut session_driver = session_runtime_driver::SessionDriver::new();
                let mut buffered_session_events = Vec::new();
                // Requests the agent made during the load, held for the user until there is
                // somewhere to ask: the replay turn owns them in the ledger meanwhile.
                let mut held_requests = Vec::new();
                let load_request_id: acp::RequestId = load_started_rx.await.map_err(|_| {
                    anyhow::anyhow!("session/load did not expose its wire request ID")
                })?;
                let mut buffered_events = client_events_to_replay_client_events(
                    session_driver
                        .apply(session_reducer::InboundEvent::LoadSubmit {
                            request_id: load_request_id.clone(),
                        })
                        .events,
                );
                let mut done = std::pin::pin!(load_done_rx);
                loop {
                    tokio::select! {
                        biased;
                        maybe_event = event_rx.recv() => {
                            match maybe_event {
                                Some(crate::connection::ConnectionEvent::Acp(event)) => {
                                    buffered_session_events.push(
                                        nori_protocol::SessionEvent::Acp(*event),
                                    );
                                }
                                Some(crate::connection::ConnectionEvent::SessionUpdate(update)) => {
                                    buffered_events.extend(client_events_to_replay_client_events(
                                        session_driver
                                            .apply(session_reducer::InboundEvent::Notification(Box::new(update)))
                                            .events,
                                    ));
                                }
                                Some(crate::connection::ConnectionEvent::Subagent(wire)) => {
                                    buffered_session_events.push(nori_protocol::SessionEvent::Nori(
                                        nori_protocol::NoriEvent::Subagent(subagent_identity::native_child(wire, &agent)),
                                    ));
                                }
                                Some(crate::connection::ConnectionEvent::SessionClosed) => break,
                                Some(crate::connection::ConnectionEvent::Barrier(reply)) => { let _ = reply.send(()); }
                                Some(crate::connection::ConnectionEvent::DelegatedRequest(request)) => {
                                    hold_load_request(&mut session_driver, &mut held_requests, request);
                                }
                                Some(crate::connection::ConnectionEvent::ChildExited { status, stderr_tail }) => {
                                    warn!(?status, %stderr_tail, "ACP agent exited during session/load");
                                    break;
                                }
                                None => break,
                            }
                        }
                        _ = &mut done => {
                            // Drain any remaining buffered updates after load completes
                            while let Ok(event) = event_rx.try_recv() {
                                match event {
                                    crate::connection::ConnectionEvent::Acp(event) => {
                                        buffered_session_events.push(
                                            nori_protocol::SessionEvent::Acp(*event),
                                        );
                                    }
                                    crate::connection::ConnectionEvent::SessionUpdate(update) => {
                                        buffered_events.extend(client_events_to_replay_client_events(
                                            session_driver
                                                .apply(session_reducer::InboundEvent::Notification(Box::new(update)))
                                                .events,
                                        ));
                                    }
                                    crate::connection::ConnectionEvent::Subagent(wire) => {
                                        buffered_session_events.push(nori_protocol::SessionEvent::Nori(
                                            nori_protocol::NoriEvent::Subagent(subagent_identity::native_child(wire, &agent)),
                                        ));
                                    }
                                    crate::connection::ConnectionEvent::SessionClosed => {}
                                    crate::connection::ConnectionEvent::Barrier(reply) => { let _ = reply.send(()); }
                                    crate::connection::ConnectionEvent::DelegatedRequest(request) => {
                                        hold_load_request(&mut session_driver, &mut held_requests, request);
                                    }
                                    crate::connection::ConnectionEvent::ChildExited { status, stderr_tail } => {
                                        warn!(?status, %stderr_tail, "ACP agent exited during session/load drain");
                                    }
                                }
                            }
                            buffered_events.extend(client_events_to_replay_client_events(
                                session_driver
                                    .apply(session_reducer::InboundEvent::LoadResponse {
                                        request_id: load_request_id,
                                    })
                                    .events,
                            ));
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>((
                    session_driver,
                    event_rx,
                    buffered_events,
                    buffered_session_events,
                    held_requests,
                ))
            });

            let mcp_servers = session_mcp_servers(
                config,
                &connection,
                &thread_goal_state,
                &backend_event_tx,
                &goal_mcp_connected,
                &goal_mcp_http_server,
                &orchestrator,
                &workflow,
            )
            .await?;

            match connection
                .load_session(sid, &cwd, mcp_servers, Some(load_started_tx))
                .await
            {
                Ok(session_id) => {
                    // Signal the collector that load is done, then collect results.
                    let _ = load_done_tx.send(());
                    let (
                        session_driver,
                        recovered_rx,
                        buffered_client_events,
                        buffered_session_events,
                        held_requests,
                    ) = collect_handle.await.map_err(|err| {
                        anyhow::anyhow!("load session collector task panicked: {err}")
                    })??;
                    if !buffered_client_events.is_empty() {
                        debug!(
                            "ACP session/load produced {} replay client events (deferred until after setup)",
                            buffered_client_events.len()
                        );
                    }
                    let load_request_id = response_request_id(&buffered_session_events)
                        .ok_or_else(|| anyhow::anyhow!("session/load produced no raw response"))?;
                    let mut setup_session_events = vec![nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionPhaseChanged(
                            nori_protocol::SessionPhase::Loading {
                                request_id: load_request_id,
                            },
                        ),
                    )];
                    let (replay_session_events, current_session_events): (Vec<_>, Vec<_>) =
                        buffered_session_events.into_iter().partition(|event| {
                            matches!(
                                event,
                                nori_protocol::SessionEvent::Acp(
                                    nori_protocol::AcpEvent::Notification(_)
                                )
                            )
                        });
                    setup_session_events.extend(current_session_events);
                    setup_session_events.push(nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionPhaseChanged(
                            nori_protocol::SessionPhase::Idle,
                        ),
                    ));
                    debug!("ACP session resumed via session/load: {sid}");
                    let canonical =
                        transcript.filter(|transcript| !transcript.event_ids.is_empty());
                    let replay_client_events = canonical
                        .map(transcript_to_replay_client_events)
                        .unwrap_or(buffered_client_events);
                    let (source, replay_session_events) = match canonical {
                        Some(transcript) => (
                            nori_protocol::ReplaySource::Transcript,
                            transcript_to_replay_session_events(transcript),
                        ),
                        None => (nori_protocol::ReplaySource::Agent, replay_session_events),
                    };
                    (
                        session_id,
                        None,
                        false,
                        None,
                        setup_session_events,
                        replay_client_events,
                        ReplayBatch::new(source, replay_session_events)
                            .into_iter()
                            .collect(),
                        recovered_rx,
                        session_driver,
                        held_requests,
                    )
                }
                Err(e) => {
                    warn!("Server-side session/load failed: {e}");
                    let _ = load_done_tx.send(());
                    let (
                        _failed_driver,
                        mut recovered_rx,
                        _,
                        buffered_session_events,
                        abandoned_requests,
                    ) = collect_handle.await.map_err(|err| {
                        anyhow::anyhow!("load session collector task panicked: {err}")
                    })??;
                    // The session they were about no longer exists to continue.
                    for request in abandoned_requests {
                        decline_load_request(request);
                    }
                    let load_request_id = response_request_id(&buffered_session_events)
                        .ok_or_else(|| {
                            anyhow::anyhow!("failed session/load produced no raw response")
                        })?;
                    let mut setup_session_events = vec![nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionPhaseChanged(
                            nori_protocol::SessionPhase::Loading {
                                request_id: load_request_id,
                            },
                        ),
                    )];
                    let (agent_replay_events, current_session_events): (Vec<_>, Vec<_>) =
                        buffered_session_events.into_iter().partition(|event| {
                            matches!(
                                event,
                                nori_protocol::SessionEvent::Acp(
                                    nori_protocol::AcpEvent::Notification(_)
                                )
                            )
                        });
                    setup_session_events.extend(current_session_events);
                    setup_session_events.push(nori_protocol::SessionEvent::Nori(
                        nori_protocol::NoriEvent::SessionPhaseChanged(
                            nori_protocol::SessionPhase::Idle,
                        ),
                    ));

                    let Some(transcript) = transcript else {
                        setup_session_events.extend(drain_setup_session_events(
                            &mut recovered_rx,
                            &config.agent,
                        )?);
                        forward_setup_session_events(&backend_event_tx, setup_session_events)
                            .await?;
                        return Err(enhance_agent_error(e, &agent_config));
                    };
                    warn!("Falling back to client-side transcript replay");

                    let mcp_servers = session_mcp_servers(
                        config,
                        &connection,
                        &thread_goal_state,
                        &backend_event_tx,
                        &goal_mcp_connected,
                        &goal_mcp_http_server,
                        &orchestrator,
                        &workflow,
                    )
                    .await?;
                    let session_id = match connection.create_session(&cwd, mcp_servers).await {
                        Ok(session_id) => session_id,
                        Err(error) => {
                            setup_session_events.extend(drain_setup_session_events(
                                &mut recovered_rx,
                                &config.agent,
                            )?);
                            forward_setup_session_events(&backend_event_tx, setup_session_events)
                                .await?;
                            return Err(enhance_agent_error(error, &agent_config));
                        }
                    };

                    if let Some(ref default_model) = config.default_model {
                        session_defaults::apply_default_model(
                            &connection,
                            &session_id,
                            default_model,
                        )
                        .await;
                    }
                    setup_session_events.extend(drain_setup_session_events(
                        &mut recovered_rx,
                        &config.agent,
                    )?);

                    let replay_events = transcript_to_replay_client_events(transcript);
                    let replay_session_events = transcript_to_replay_session_events(transcript);
                    let summary_text = transcript_to_summary(transcript);
                    let summary = if summary_text.is_empty() {
                        None
                    } else {
                        Some(summary_text)
                    };
                    let mut replay_batches = if transcript.event_ids.is_empty() {
                        ReplayBatch::new(nori_protocol::ReplaySource::Agent, agent_replay_events)
                            .into_iter()
                            .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    if let Some(transcript_batch) = ReplayBatch::new(
                        nori_protocol::ReplaySource::Transcript,
                        replay_session_events,
                    ) {
                        replay_batches.push(transcript_batch);
                    }

                    (
                        session_id,
                        summary,
                        true,
                        Some(e.to_string()),
                        setup_session_events,
                        replay_events,
                        replay_batches,
                        recovered_rx,
                        session_runtime_driver::SessionDriver::new(),
                        Vec::new(),
                    )
                }
            }
        } else if let Some(sid) = acp_session_id.filter(|_| supports_session_resume) {
            debug!("Agent supports session/resume — using live reattach");

            // Live reattach (`session/resume`) costs the same however long the session is: the
            // agent keeps its context and replays nothing. When the local transcript is at hand
            // the history is painted from it, which is preferred over `session/load` because the
            // agent would otherwise stream the whole session back before anything is shown.
            // Failures propagate instead of falling back to `session/new`,
            // which would abandon the session the user selected.
            let mcp_servers = session_mcp_servers(
                config,
                &connection,
                &thread_goal_state,
                &backend_event_tx,
                &goal_mcp_connected,
                &goal_mcp_http_server,
                &orchestrator,
                &workflow,
            )
            .await?;

            let session_id = match connection.resume_session(sid, &cwd, mcp_servers).await {
                Ok(session_id) => session_id,
                Err(error) => {
                    let setup_events = drain_setup_session_events(&mut event_rx, &config.agent)?;
                    forward_setup_session_events(&backend_event_tx, setup_events).await?;
                    return Err(enhance_agent_error(error, &agent_config));
                }
            };
            debug!("ACP session resumed via session/resume: {sid}");
            let setup_session_events = drain_setup_session_events(&mut event_rx, &config.agent)?;
            let history = transcript
                .and_then(|transcript| {
                    ReplayBatch::new(
                        nori_protocol::ReplaySource::Transcript,
                        transcript_to_replay_session_events(transcript),
                    )
                })
                .into_iter()
                .collect();

            (
                session_id,
                None,
                false,
                None,
                setup_session_events,
                Vec::new(),
                history,
                event_rx,
                session_runtime_driver::SessionDriver::new(),
                Vec::new(),
            )
        } else {
            debug!(
                "Agent supports neither session/load nor session/resume — using client-side replay"
            );

            let mcp_servers = session_mcp_servers(
                config,
                &connection,
                &thread_goal_state,
                &backend_event_tx,
                &goal_mcp_connected,
                &goal_mcp_http_server,
                &orchestrator,
                &workflow,
            )
            .await?;
            let session_id = match connection.create_session(&cwd, mcp_servers).await {
                Ok(session_id) => session_id,
                Err(error) => {
                    let setup_events = drain_setup_session_events(&mut event_rx, &config.agent)?;
                    forward_setup_session_events(&backend_event_tx, setup_events).await?;
                    return Err(enhance_agent_error(error, &agent_config));
                }
            };

            if let Some(ref default_model) = config.default_model {
                session_defaults::apply_default_model(&connection, &session_id, default_model)
                    .await;
            }
            let setup_session_events = drain_setup_session_events(&mut event_rx, &config.agent)?;

            let (replay_events, replay_session_events, summary) = if let Some(t) = transcript {
                let client_events = transcript_to_replay_client_events(t);
                let session_events = transcript_to_replay_session_events(t);
                let summary_text = transcript_to_summary(t);
                let summary_opt = if summary_text.is_empty() {
                    None
                } else {
                    Some(summary_text)
                };
                (client_events, session_events, summary_opt)
            } else {
                (Vec::new(), Vec::new(), None)
            };

            (
                session_id,
                summary,
                true,
                None,
                setup_session_events,
                replay_events,
                ReplayBatch::new(
                    nori_protocol::ReplaySource::Transcript,
                    replay_session_events,
                )
                .into_iter()
                .collect(),
                event_rx,
                session_runtime_driver::SessionDriver::new(),
                Vec::new(),
            )
        };

        if let Some(transcript) = transcript
            && !deferred_replay_batches
                .iter()
                .any(|batch| batch.source == nori_protocol::ReplaySource::Transcript)
        {
            let history = transcript_to_replay_session_events(transcript)
                .into_iter()
                .filter(|event| {
                    matches!(
                        event,
                        SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(_))
                            | SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(_))
                    )
                })
                .collect();
            deferred_replay_batches.extend(ReplayBatch::new(
                nori_protocol::ReplaySource::Transcript,
                history,
            ));
        }

        if !deferred_replay_client_events.is_empty() {
            let mut replay_events_for_goal_state = initial_goal_replay_events;
            replay_events_for_goal_state.extend(deferred_replay_client_events.iter().cloned());
            *thread_goal_state.lock().await =
                thread_goal::ThreadGoalState::from_replay_events(&replay_events_for_goal_state);
        }

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
        let native_children = Arc::new(super::native_child_sessions::NativeChildren::default());
        if let Some(transcript) = transcript {
            for line in &transcript.entries {
                if let crate::transcript::TranscriptEntry::SessionEvent(entry) = &line.entry
                    && !entry.historical_evidence
                    && let SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(wire)) =
                        &entry.event
                {
                    native_children
                        .observe_replayed(&connection, &backend_event_tx, &session_id, wire)
                        .await;
                } else if let crate::transcript::TranscriptEntry::SessionEvent(entry) = &line.entry
                    && !entry.historical_evidence
                    && let SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(
                        notification,
                    )) = &entry.event
                {
                    let _ = native_children.observe_notification(notification).await;
                }
            }
        }
        for batch in &deferred_replay_batches {
            for event in &batch.events {
                if let SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(wire)) = event {
                    if batch.source == nori_protocol::ReplaySource::Transcript {
                        native_children
                            .observe_replayed(&connection, &backend_event_tx, &session_id, wire)
                            .await;
                    } else {
                        let _ = native_children
                            .observe(&connection, &backend_event_tx, &session_id, wire)
                            .await;
                    }
                } else if let SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(
                    notification,
                )) = event
                {
                    let _ = native_children.observe_notification(notification).await;
                }
            }
        }
        let mut fresh_subagent_events = initial_setup_events
            .iter()
            .chain(&deferred_setup_session_events)
            .filter(|event| {
                matches!(
                    event,
                    SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(_))
                )
            })
            .cloned()
            .collect::<Vec<_>>();
        for event in initial_setup_events
            .iter()
            .chain(&deferred_setup_session_events)
        {
            match event {
                SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(wire)) => {
                    let _ = native_children
                        .observe(&connection, &backend_event_tx, &session_id, wire)
                        .await;
                }
                SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(notification)) => {
                    let _ = native_children.observe_notification(notification).await;
                }
                _ => {}
            }
        }
        let pending_approvals = Arc::new(Mutex::new(Vec::new()));
        let session_driver = Arc::new(Mutex::new(session_driver_state));
        let (session_event_tx, session_event_rx) = mpsc::channel(128);
        let (prompt_result_tx, prompt_result_rx) = mpsc::channel(128);
        let use_native_notifications =
            config.os_notifications == crate::config::OsNotifications::Enabled;
        let user_notifier = Arc::new(crate::UserNotifier::new(
            config.notify.clone(),
            use_native_notifications,
        ));
        let idle_timer_abort = Arc::new(Mutex::new(None));
        let (approval_policy_tx, approval_policy_rx) = watch::channel(config.approval_policy);
        let (history_log_id, history_entry_count) =
            crate::message_history::history_metadata(&config.nori_home).await;

        // Every session owns a canonical transcript. A resumed session whose history is already
        // recorded continues that transcript; agent-side replay starts a fresh transcript so it
        // does not repeat history that may be newer than the local copy.
        let agent_replays = deferred_replay_batches
            .iter()
            .any(|batch| batch.source == nori_protocol::ReplaySource::Agent);
        if let Some(transcript) = transcript
            && transcript.meta.delegated
            && !config.delegation.is_child()
        {
            anyhow::bail!(
                "transcript {} belongs to a delegated child session, not a conversation of its own; \
                 resume the session that delegated it",
                transcript.meta.session_id
            );
        }
        let recorded_transcript = transcript
            .filter(|_| !agent_replays)
            .map(|transcript| {
                let path = crate::transcript::TranscriptLoader::new(config.nori_home.clone())
                    .session_path(&transcript.meta.project_id, &transcript.meta.session_id);
                (path, &transcript.meta)
            })
            .filter(|(path, _)| path.is_file());
        let transcript_recorder = if !config.delegation.record_transcript {
            None
        } else {
            let recorder = match recorded_transcript {
                Some((path, meta)) => TranscriptRecorder::continue_session(path, meta).await,
                None => {
                    TranscriptRecorder::new_for(
                        &config.nori_home,
                        &cwd,
                        Some(config.agent.clone()),
                        &config.cli_version,
                        Some(session_id.to_string()),
                        config.delegation.is_child(),
                    )
                    .await
                }
            };
            Some(Arc::new(recorder.map_err(|error| {
                anyhow::anyhow!("failed to initialize canonical transcript: {error}")
            })?))
        };
        // A new transcript of a resumed session starts with the evidence of the old one.
        if let (Some(recorder), Some(transcript)) = (&transcript_recorder, transcript)
            && !recorder.continues_recorded_session()
        {
            for evidence in transcript.archived_context_evidence() {
                if let Err(error) = recorder.record_session_event(&evidence).await {
                    warn!(%error, "Failed to archive context timeline evidence");
                    break;
                }
            }
        }
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
        if let Some(recorder) = &transcript_recorder
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
            warn!(%error, "Failed to record resumed backend binding");
        }
        let pending_hook_context = orchestrator
            .initial_catalog_context(
                session_context_for_connection(config, &connection),
                &workflow,
                goal_mcp_http_server.lock().await.is_some(),
            )
            .await;
        let goal_ext_capability =
            goal_ext::GoalExtCapability::from_initialize_meta(connection.initialize_meta());
        let transcript_recorder_cell = Arc::new(RwLock::new(transcript_recorder));
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
        if let Some(transcript) = transcript
            && transcript_recorder_cell
                .read()
                .await
                .as_ref()
                .is_some_and(|recorder| recorder.has_conversation_log())
        {
            native_children.bootstrap_history(transcript).await?;
        }
        let backend = Self {
            connection,
            session_id: Arc::new(RwLock::new(session_id)),
            backend_event_tx: backend_event_tx.clone(),
            cwd: cwd.clone(),
            pending_approvals: Arc::clone(&pending_approvals),
            withdrawn_questions: Arc::new(Mutex::new(std::collections::HashSet::new())),
            pending_prompt_submissions: Arc::new(Mutex::new(HashMap::new())),
            user_notifier: Arc::clone(&user_notifier),
            idle_timer_abort: Arc::clone(&idle_timer_abort),
            nori_home: config.nori_home.clone(),
            history_persistence: config.history_persistence,
            acp_proxy: config.acp_proxy.clone(),
            conversation_id: Arc::new(RwLock::new(conversation_id)),
            approval_policy_tx,
            compact_nudged: Arc::new(AtomicBool::new(false)),
            pending_compact_summary: Arc::new(Mutex::new(
                pending_summary.map(|summary| crate::compact::checkpoint(&summary, None)),
            )),
            thread_goal_state,
            goal_mcp_connected,
            goal_mcp_http_server,
            goal_ext_capability,
            pending_hook_context: Arc::new(Mutex::new(pending_hook_context)),
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
            is_first_prompt: Arc::new(Mutex::new(is_first_prompt_val)),
            workflow,
            machine_snapshot,
            workflow_ready: tokio::sync::watch::channel(
                policy_before_session || config.delegation.is_child(),
            )
            .0,
            program_effects: Arc::new(tokio::sync::RwLock::new(())),
            prompt_summary_enabled: config.prompt_summary_enabled,
            agent_name: config.agent.clone(),
            handoff_candidate: false,
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
        // Before the agent can be asked anything: what it armed is what `monitor_list` says.
        backend.restore_monitors().await;
        backend.serve_orchestrator(orchestrator_signals);
        let previous_root =
            transcript.and_then(crate::transcript::Transcript::latest_acp_session_id);
        // An ordinary resume already has its local journals: do not rebuild every child and
        // replay its history inside the connection deadline. A newly imported conversation needs
        // this once on its new machine, and records completion so later resumes stay fast.
        let mut restored = Vec::new();
        if backend.needs_moved_delegation_restore().await {
            restored = backend
                .restore_native_children(previous_root.as_deref())
                .await?;
            restored.extend(
                backend
                    .restore_managed_children(previous_root.as_deref())
                    .await?,
            );
            backend.mark_moved_delegation_restored().await;
        }
        for child in restored {
            let id = child.id.clone();
            let related = |event: &SessionEvent| match event {
                SessionEvent::Acp(nori_protocol::AcpEvent::ChildNotification(notification)) => {
                    child.retired.contains(&notification.session_id)
                }
                SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Update(update),
                )) => child.retired.contains(&update.session_id),
                SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                    nori_protocol::SubagentWire::Message(message),
                )) => {
                    message
                        .sender_session_id
                        .as_ref()
                        .is_some_and(|id| child.retired.contains(id))
                        || message
                            .recipient_session_id
                            .as_ref()
                            .is_some_and(|id| child.retired.contains(id))
                }
                SessionEvent::Nori(nori_protocol::NoriEvent::SubagentQueueChanged {
                    session_id,
                    ..
                }) => child.retired.contains(session_id),
                _ => false,
            };
            let saved = deferred_setup_session_events
                .iter()
                .chain(
                    deferred_replay_batches
                        .iter()
                        .flat_map(|batch| &batch.events),
                )
                .find_map(|event| match event {
                    SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                        nori_protocol::SubagentWire::Update(update),
                    )) if update.session_id == id => Some(update.clone()),
                    _ => None,
                });
            deferred_setup_session_events.retain(|event| !related(event));
            fresh_subagent_events.retain(|event| !related(event));
            for batch in &mut deferred_replay_batches {
                batch.events.retain(|event| !related(event));
            }
            let mut announcement = saved.unwrap_or(nori_protocol::SubagentUpdate {
                parent_session_id: child.parent.clone(),
                session_id: id.clone(),
                agent: nori_protocol::Patch::Keep,
                model: nori_protocol::Patch::Keep,
                title: nori_protocol::Patch::Keep,
                description: nori_protocol::Patch::Keep,
                state: nori_protocol::Patch::Keep,
            });
            announcement.parent_session_id = child.parent;
            announcement.state = nori_protocol::Patch::Set(child.state);
            let announcement = SessionEvent::Nori(nori_protocol::NoriEvent::Subagent(
                nori_protocol::SubagentWire::Update(announcement),
            ));
            deferred_setup_session_events.push(announcement.clone());
            fresh_subagent_events.push(announcement);
            let replay = child
                .events
                .into_iter()
                .filter_map(|event| match event {
                    SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                        acp::AgentNotification::SessionNotification(mut notification),
                    )) => {
                        notification.session_id = id.clone();
                        Some(SessionEvent::Acp(
                            nori_protocol::AcpEvent::ChildNotification(notification),
                        ))
                    }
                    _ => None,
                })
                .collect();
            deferred_replay_batches.extend(ReplayBatch::new(
                nori_protocol::ReplaySource::Transcript,
                replay,
            ));
        }
        forward_setup_session_events(&backend_event_tx, deferred_setup_session_events).await?;
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
                nori_protocol::NoriEvent::CapabilitiesChanged(public_nori_capabilities(
                    capabilities_update,
                )),
            )))
            .await
            .ok();

        if let Some(ref fallback_error) = used_fallback {
            backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    nori_protocol::NoriEvent::Notice(nori_protocol::Notice {
                        message: format!(
                            "Server-side session restore failed ({fallback_error}). \
                             Falling back to transcript replay. The restored session \
                             will not have tool call information in the context."
                        ),
                    }),
                )))
                .await
                .ok();
        }

        let goal_automation_available = backend.goal_mcp_http_server.lock().await.is_some()
            || backend.goal_ext_capability().is_some();
        let resume_goal_notice = {
            let goals = backend.thread_goal_state.lock().await;
            let now = thread_goal::now_seconds();
            goals.resume_notice_for(now, goal_automation_available)
        };

        let replay_session_id = backend.session_id().await;
        let has_replay = !deferred_replay_batches.is_empty();
        let relay_backend = backend.clone();
        let relay_task = tokio::spawn(async move {
            for ReplayBatch { source, events } in deferred_replay_batches {
                let _ = relay_backend
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::ReplayStarted(nori_protocol::ReplayStarted {
                            source,
                        }),
                    )))
                    .await;
                for event in events {
                    let _ = relay_backend
                        .backend_event_tx
                        .send(BackendEvent::Public(retarget_replay_event(
                            event,
                            &replay_session_id,
                        )))
                        .await;
                }
                let _ = relay_backend
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::ReplayFinished,
                    )))
                    .await;
            }
            let _ = relay_backend
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    nori_protocol::NoriEvent::SessionReady(replay_session_id.clone()),
                )))
                .await;
            if has_replay {
                for event in fresh_subagent_events {
                    let _ = relay_backend
                        .backend_event_tx
                        .send(BackendEvent::Public(event))
                        .await;
                }
            }
            if let Some(update) = resume_goal_notice {
                let _ = relay_backend
                    .backend_event_tx
                    .send(BackendEvent::Public(SessionEvent::Nori(
                        nori_protocol::NoriEvent::Notice(nori_protocol::Notice {
                            message: match update.hint {
                                Some(hint) => format!("{} {hint}", update.message),
                                None => update.message,
                            },
                        }),
                    )))
                    .await;
            }
            // The requests the agent made during the load wait no longer: the ledger already
            // holds them in the turn that continues the load, and the user is asked now.
            for request in held_requests {
                let policy = *approval_policy_rx.borrow();
                relay_backend
                    .handle_permission_request(Box::new(PendingApprovalRequest { request }), policy)
                    .await;
            }
            // What the previous session had queued comes back held: the user sends or drops it.
            relay_backend.restore_queued_prompts().await;
            Self::run_connection_event_relay(
                relay_backend,
                event_rx,
                prompt_result_rx,
                approval_policy_rx,
            )
            .await;
        });
        *backend.relay_task_abort.lock().await = Some(relay_task.abort_handle());

        Ok(backend)
    }

    /// Close (release) the active session via ACP `session/close`.
    ///
    /// Used by the explicit close action; callers gate on the agent
    /// advertising `sessionCapabilities.close`. Failures carry the same
    /// enhanced structured-code-aware message as spawn/resume errors, so the
    /// agent's error code and `data.detail` reach the user.
    pub async fn close_active_session(&self) -> Result<()> {
        let session_id = self.session_id.read().await.clone();
        self.connection
            .close_session(&session_id.to_string())
            .await
            .map_err(|e| match get_agent_config(&self.agent_name) {
                Ok(agent_config) => enhance_agent_error(e, &agent_config),
                Err(_) => e,
            })?;
        self.teardown(false, None).await;
        Ok(())
    }
}

/// Hold a request the agent made during `session/load`. The ledger's replay turn owns it, and it
/// waits for the backend's approval channel instead of being declined before anyone could be
/// asked.
fn hold_load_request(
    driver: &mut session_runtime_driver::SessionDriver,
    held: &mut Vec<crate::connection::DelegatedRequest>,
    request: crate::connection::DelegatedRequest,
) {
    let (call_id, title) = match &request.request {
        acp::AgentRequest::RequestPermissionRequest(permission) => (
            Some(permission.tool_call.tool_call_id.to_string()),
            permission
                .tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| "tool request".to_string()),
        ),
        acp::AgentRequest::ExtMethodRequest(ext)
            if ext.method.as_ref() == "_nori/request_user_input" =>
        {
            (None, "question".to_string())
        }
        _ => {
            let _ = request
                .response_tx
                .send(Err(acp::Error::method_not_found()));
            return;
        }
    };
    driver.apply(session_reducer::InboundEvent::PermissionRequest {
        request_id: request.request_id.to_string(),
        call_id,
        title,
    });
    held.push(request);
}

/// Answer a held load request as cancelled because the session it belonged to is gone.
fn decline_load_request(request: crate::connection::DelegatedRequest) {
    let _ = request
        .response_tx
        .send(Ok(acp::ClientResponse::RequestPermissionResponse(
            acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
        )));
}
