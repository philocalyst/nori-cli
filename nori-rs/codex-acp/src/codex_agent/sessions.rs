//! Session lifecycle: starting, restoring, listing, prompting, cancelling and closing Codex threads.
//!
//! A session is a [`Thread`] held in the agent's session table. Codex's own subagent threads have
//! no entry there; they are reached through the child-prompt channels their forwarder registers.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::PoisonError;

use codex_core::NewThread;
use codex_core::RolloutRecorder;
use codex_core::find_thread_path_by_id_str;
use codex_core_api::StartThreadOptions;
use codex_history::InitialHistory;
use codex_protocol::ThreadId;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::protocol::SessionSource;
use codex_thread_store::ListThreadsParams;
use codex_thread_store::SortDirection as StoreSortDirection;
use codex_thread_store::ThreadSortKey as StoreThreadSortKey;
use nori_acp_kit::AgentError;
use nori_acp_kit::Cx;
use nori_acp_kit::acp;
use nori_acp_kit::schema::CloseSessionRequest;
use nori_acp_kit::schema::CloseSessionResponse;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionInfo;
use nori_acp_kit::schema::StopReason;
use nori_host_tools::HostTools;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::info;

use super::CodexAgent;
use super::titles::stored_session_title;
use crate::thread::ChildPrompt;
use crate::thread::LiveModels;
use crate::thread::Thread;

const SESSION_LIST_PAGE_SIZE: usize = 25;

/// `options` with the host's tools as dynamic tools and their instructions as the thread's.
fn with_host_tools(
    mut options: StartThreadOptions,
    tools: Option<&Arc<dyn HostTools>>,
) -> StartThreadOptions {
    if let Some(tools) = tools {
        options.dynamic_tools = crate::host_tools::dynamic_tools(tools.as_ref());
        options.thread_instructions_provider = Some(Arc::new(crate::host_tools::HostInstructions(
            Arc::clone(tools),
        )));
    }
    options
}

impl CodexAgent {
    /// The host's tools a session request's `_meta` names, if it names registered ones.
    fn host_tools(
        &self,
        meta: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Option<Arc<dyn HostTools>> {
        nori_host_tools::from_session_meta(meta)
    }

    /// The agent-side handle of a started Codex thread, ready for the client's messages.
    fn attach(
        &self,
        session_id: SessionId,
        thread: Arc<codex_core::CodexThread>,
        config: codex_core::config::Config,
        host_tools: Option<Arc<dyn HostTools>>,
        cx: &Cx,
    ) -> Thread {
        Thread::new(
            session_id,
            thread,
            self.auth_manager.clone(),
            Arc::new(LiveModels::new(
                self.thread_manager.get_models_manager(),
                self.config.http_client_factory(),
            )),
            self.client_capabilities.clone(),
            config,
            host_tools,
            cx.clone(),
            Arc::clone(&self.thread_manager),
            Arc::clone(&self.child_prompts),
        )
    }

    /// Send `command` to the forwarder of Codex subagent thread `session` and wait for its answer.
    async fn ask_child<T>(
        &self,
        session: &SessionId,
        command: impl FnOnce(oneshot::Sender<acp::Result<T>>) -> ChildPrompt,
    ) -> acp::Result<T> {
        let sender = self
            .child_prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(session)
            .cloned()
            .ok_or_else(|| AgentError::unknown_session(session))?;
        let (response_tx, response_rx) = oneshot::channel();
        sender
            .send(command(response_tx))
            .map_err(|_| AgentError::internal("the subagent session has ended"))?;
        response_rx.await.map_err(AgentError::internal)?
    }

    pub(super) async fn start_session(
        &self,
        cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        // Check before sending if authentication was successful or not
        self.check_auth().await?;
        self.announce_identity(cx).await;

        let NewSessionRequest {
            cwd,
            mcp_servers,
            meta,
            ..
        } = request;
        info!("Creating new session with cwd: {}", cwd.display());

        let config = self.build_session_config(&cwd, mcp_servers)?;
        let num_mcp_servers = config.mcp_servers.len();
        let host_tools = self.host_tools(meta.as_ref());

        let NewThread {
            thread_id,
            thread,
            session_configured: _,
        } = Box::pin(self.thread_manager.start_thread(with_host_tools(
            StartThreadOptions::new(config.clone()),
            host_tools.as_ref(),
        )))
        .await
        .map_err(|error| {
            AgentError::internal(format!("could not start a Codex thread: {error}"))
        })?;

        let session_id = SessionId::new(thread_id.to_string());
        if let Some(tools) = &host_tools {
            tools.bind_session_id(&session_id.to_string());
        }
        let thread = self.attach(session_id.clone(), thread, config, host_tools, cx);
        let load = thread.load().await?;
        self.sessions.insert(session_id.clone(), thread);

        debug!("Created new session with {} MCP servers", num_mcp_servers);

        Ok(NewSessionResponse::new(session_id)
            .modes(load.modes)
            .config_options(load.config_options))
    }

    pub(super) async fn load(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        info!("Loading session: {}", request.session_id);
        self.check_auth().await?;

        let LoadSessionRequest {
            session_id,
            cwd,
            mcp_servers,
            meta,
            ..
        } = request;
        let host_tools = self.host_tools(meta.as_ref());
        self.restore_session(session_id, cwd, mcp_servers, host_tools, cx, true)
            .await
    }

    pub(super) async fn resume(
        &self,
        cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        info!("Resuming session: {}", request.session_id);
        self.check_auth().await?;

        let ResumeSessionRequest {
            session_id,
            cwd,
            mcp_servers,
            meta,
            ..
        } = request;
        let host_tools = self.host_tools(meta.as_ref());
        let load = self
            .restore_session(session_id, cwd, mcp_servers, host_tools, cx, false)
            .await?;

        Ok(ResumeSessionResponse::new()
            .modes(load.modes)
            .config_options(load.config_options))
    }

    async fn restore_session(
        &self,
        session_id: SessionId,
        cwd: PathBuf,
        mcp_servers: Vec<McpServer>,
        host_tools: Option<Arc<dyn HostTools>>,
        cx: &Cx,
        replay_history: bool,
    ) -> acp::Result<LoadSessionResponse> {
        let rollout_path = find_thread_path_by_id_str(
            &self.config.codex_home,
            session_id.0.as_ref(),
            self.state_db.as_deref(),
        )
        .await
        .map_err(AgentError::internal)?
        .ok_or_else(|| AgentError::unknown_session(&session_id))?;

        let history = RolloutRecorder::get_rollout_history(&rollout_path)
            .await
            .map_err(AgentError::internal)?;
        let rollout_items = if replay_history {
            match &history {
                InitialHistory::Resumed(resumed) => resumed.history.to_vec(),
                InitialHistory::Forked(items) => items.clone(),
                InitialHistory::Cleared | InitialHistory::New => Vec::new(),
            }
        } else {
            Vec::new()
        };

        let config = self.build_session_config(&cwd, mcp_servers)?;

        // As `resume_thread_with_history` does, plus the host's tools: a cold resume must be given
        // them, and their instructions, again.
        let NewThread {
            thread_id: _,
            thread,
            session_configured: _,
        } = Box::pin(self.thread_manager.start_thread(with_host_tools(
            StartThreadOptions {
                initial_history: history,
                client_mcp_extensions: ClientMcpExtensions::default(),
                ..StartThreadOptions::new(config.clone())
            },
            host_tools.as_ref(),
        )))
        .await
        .map_err(AgentError::internal)?;

        if let Some(tools) = &host_tools {
            tools.bind_session_id(&session_id.to_string());
        }

        let thread = self.attach(session_id.clone(), thread, config, host_tools, cx);
        if replay_history {
            thread.replay_history(rollout_items).await?;
        }
        let load = thread.load().await?;
        self.sessions.insert(session_id, thread);

        Ok(LoadSessionResponse::new()
            .modes(load.modes)
            .config_options(load.config_options))
    }

    pub(super) async fn list(
        &self,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        self.check_auth().await?;

        let ListSessionsRequest { cwd, cursor, .. } = request;
        let allowed_sources = [
            SessionSource::Cli,
            SessionSource::VSCode,
            SessionSource::Unknown,
        ];
        let cwd_filter = cwd.clone();

        let page = self
            .thread_store
            .list_threads(ListThreadsParams {
                page_size: SESSION_LIST_PAGE_SIZE,
                cursor,
                sort_key: StoreThreadSortKey::UpdatedAt,
                sort_direction: StoreSortDirection::Desc,
                allowed_sources: allowed_sources.to_vec(),
                model_providers: None,
                cwd_filters: cwd.map(|cwd| vec![cwd]),
                section: None,
                project_id: None,
                archived: false,
                search_term: None,
                relation_filter: None,
                use_state_db_only: false,
            })
            .await
            .map_err(|error| AgentError::internal(format!("failed to list sessions: {error}")))?;

        let sessions = page
            .items
            .into_iter()
            .filter(|item| {
                allowed_sources.contains(&item.source)
                    && cwd_filter
                        .as_ref()
                        .is_none_or(|filter_cwd| item.cwd.as_path() == filter_cwd.as_path())
            })
            .map(|item| {
                let title = stored_session_title(item.name.as_deref(), &item.preview);
                let updated_at = item.updated_at.to_rfc3339();

                SessionInfo::new(SessionId::new(item.thread_id.to_string()), item.cwd)
                    .title(title)
                    .updated_at(updated_at)
            })
            .collect::<Vec<_>>();

        Ok(ListSessionsResponse::new(sessions).next_cursor(page.next_cursor))
    }

    pub(super) async fn close(
        &self,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        if let Some(thread) = self.sessions.get(&request.session_id) {
            thread.shutdown().await?;
        } else {
            self.ask_child(&request.session_id, ChildPrompt::Close)
                .await?;
        }
        self.thread_manager
            .remove_thread(
                &ThreadId::from_string(&request.session_id.0)
                    .map_err(|error| AgentError::invalid(error.to_string()))?,
            )
            .await;
        self.sessions.remove(&request.session_id);

        Ok(CloseSessionResponse::new())
    }

    pub(super) async fn run_prompt(&self, request: PromptRequest) -> acp::Result<StopReason> {
        info!("Processing prompt for session: {}", request.session_id);
        self.check_auth().await?;

        match self.sessions.get(&request.session_id) {
            Some(thread) => thread.prompt(request).await,
            None => {
                let session = request.session_id.clone();
                self.ask_child(&session, |response_tx| {
                    ChildPrompt::Prompt(request, response_tx)
                })
                .await
            }
        }
    }

    pub(super) async fn interrupt(&self, session: &SessionId) -> acp::Result<()> {
        info!("Cancelling operations for session: {session}");
        match self.sessions.get(session) {
            Some(thread) => thread.cancel().await,
            None => self.ask_child(session, ChildPrompt::Cancel).await,
        }
    }
}
