//! Codex as a [`NativeAgent`]: one `CodexAgent` owns the Codex thread manager and serves every
//! session of the connection. The kit serves the connection; the work is in the submodules.
//!
//! - `sessions`: starting, restoring, listing, prompting, cancelling and closing sessions.
//! - `session_config`: the Codex configuration of a session, with the client's MCP servers.
//! - `auth`: the authentication methods and signing in.
//! - `titles`: how stored threads are titled in the session list.

mod auth;
mod session_config;
mod sessions;
mod titles;

use std::sync::Arc;
use std::sync::Mutex;

use codex_core::StateDbHandle;
use codex_core::ThreadManager;
use codex_core::config::Config;
use codex_core::init_state_db;
use codex_core::resolve_installation_id;
use codex_core_api::CodexAppsToolsCache;
use codex_core_api::CodexHomeUserInstructionsProvider;
use codex_core_api::EnvironmentManager;
use codex_core_api::ExecServerRuntimeOptions;
use codex_core_api::ExtensionRegistryBuilder;
use codex_core_api::ModelCatalogSettings;
use codex_core_api::ThreadStoreBuilder;
use codex_core_api::build_startup_models;
use codex_core_api::install_image_generation_extension;
use codex_core_api::local_agent_graph_store_from_state_db;
use codex_core_api::passthrough_image_store;
use codex_login::auth::AuthManager;
use codex_protocol::protocol::SessionSource;
use codex_thread_store::ThreadStore;
use nori_acp_kit::Capabilities;
use nori_acp_kit::Cx;
use nori_acp_kit::NativeAgent;
use nori_acp_kit::Profile;
use nori_acp_kit::Sessions;
use nori_acp_kit::acp;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::AuthenticateRequest;
use nori_acp_kit::schema::AuthenticateResponse;
use nori_acp_kit::schema::ClientCapabilities;
use nori_acp_kit::schema::CloseSessionRequest;
use nori_acp_kit::schema::CloseSessionResponse;
use nori_acp_kit::schema::InitializeRequest;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::LogoutRequest;
use nori_acp_kit::schema::LogoutResponse;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SetSessionConfigOptionRequest;
use nori_acp_kit::schema::SetSessionConfigOptionResponse;
use nori_acp_kit::schema::SetSessionModeRequest;
use nori_acp_kit::schema::SetSessionModeResponse;
use tracing::debug;
use tracing::info;

use crate::thread::ChildPrompts;
use crate::thread::Thread;

/// The Codex implementation of the ACP agent: it bridges ACP onto the Codex core, which runs in
/// this process.
pub struct CodexAgent {
    /// Handle to the current authentication
    auth_manager: Arc<AuthManager>,
    /// Capabilities of the connected client
    client_capabilities: Arc<Mutex<ClientCapabilities>>,
    /// The underlying codex configuration
    config: Config,
    /// Thread manager for handling sessions
    thread_manager: Arc<ThreadManager>,
    /// The forwarders of Codex subagent threads, by the session id each is shown under.
    child_prompts: ChildPrompts,
    /// Store for listing and updating persisted thread metadata
    thread_store: Arc<dyn ThreadStore>,
    /// SQLite-backed Codex state index, when initialization succeeds
    state_db: Option<StateDbHandle>,
    /// Active sessions
    sessions: Sessions<Thread>,
}

impl CodexAgent {
    /// Create a `CodexAgent` over `config`.
    pub async fn new(config: Config) -> std::io::Result<Self> {
        let auth_manager =
            AuthManager::shared_from_config(&config, /*enable_codex_api_key_env*/ false)
                .await
                .map_err(std::io::Error::other)?;

        let state_db = init_state_db(&config).await;
        // The process that hosts this agent is the executable commands re-enter for `apply_patch`.
        let local_runtime_paths = ExecServerRuntimeOptions::from_optional_paths(
            config
                .codex_self_exe
                .clone()
                .or_else(|| std::env::current_exe().ok()),
            config.codex_linux_sandbox_exe.clone(),
        )?;
        let thread_store = ThreadStoreBuilder::from_config(&config, state_db.clone()).build();
        let environment_manager = Arc::new(
            EnvironmentManager::from_codex_home(
                config.codex_home.clone(),
                Some(local_runtime_paths),
                config.http_client_factory(),
            )
            .await
            .map_err(std::io::Error::other)?,
        );
        let installation_id = resolve_installation_id(&config.codex_home).await?;
        let user_instructions_provider = Arc::new(CodexHomeUserInstructionsProvider::new(
            config.codex_home.clone(),
        ));
        let mut extensions = ExtensionRegistryBuilder::<Config>::new();
        install_image_generation_extension(
            &mut extensions,
            auth_manager.clone(),
            |config: &Config| Some(config.codex_home.clone()),
        );
        let startup = build_startup_models(
            &config,
            Arc::clone(&auth_manager),
            installation_id,
            Arc::new(ModelCatalogSettings::disk(config.codex_home.to_path_buf())),
        );
        let thread_manager = Arc::new(ThreadManager::new(
            &config,
            startup,
            CodexAppsToolsCache::default(),
            SessionSource::Unknown,
            environment_manager,
            Arc::new(extensions.build()),
            user_instructions_provider,
            /*analytics_events_client*/ None,
            passthrough_image_store(),
            Arc::clone(&thread_store),
            local_agent_graph_store_from_state_db(state_db.as_ref()),
            /*attestation_provider*/ None,
            /*external_time_provider*/ None,
        ));
        Ok(Self {
            auth_manager,
            client_capabilities: Arc::default(),
            config,
            thread_manager,
            child_prompts: Arc::default(),
            thread_store,
            state_db,
            sessions: Sessions::default(),
        })
    }
}

#[async_trait]
impl NativeAgent for CodexAgent {
    fn profile(&self) -> Profile {
        // The wire name is what Nori keys Codex-specific behaviour on; keep it.
        Profile::new("codex-acp", "Codex", env!("CARGO_PKG_VERSION"))
            .capabilities(Capabilities {
                image: true,
                embedded_context: true,
                mcp_http: true,
                load: true,
                resume: true,
                list: true,
                close: true,
                logout: true,
                host_tools: true,
                ..Capabilities::default()
            })
            .auth_methods(auth::auth_methods())
    }

    fn on_initialize(&self, request: &InitializeRequest) {
        debug!(
            "Received initialize request with protocol version {:?}",
            request.protocol_version
        );
        *self
            .client_capabilities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            request.client_capabilities.clone();
    }

    async fn authenticate(
        &self,
        _cx: &Cx,
        request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        self.sign_in(request).await
    }

    async fn logout(&self, _cx: &Cx, _request: LogoutRequest) -> acp::Result<LogoutResponse> {
        self.sign_out().await
    }

    async fn new_session(
        &self,
        cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        self.start_session(cx, request).await
    }

    async fn load_session(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        self.load(cx, request).await
    }

    async fn resume_session(
        &self,
        cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        self.resume(cx, request).await
    }

    async fn list_sessions(
        &self,
        _cx: &Cx,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        self.list(request).await
    }

    async fn close_session(
        &self,
        _cx: &Cx,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        self.close(request).await
    }

    async fn set_session_mode(
        &self,
        _cx: &Cx,
        request: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        info!("Setting session mode for session: {}", request.session_id);
        self.sessions
            .require(&request.session_id)?
            .set_mode(request.mode_id)
            .await?;
        Ok(SetSessionModeResponse::default())
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        info!(
            "Setting session config option for session: {} (config_id: {}, value: {:?})",
            request.session_id, request.config_id.0, request.value
        );
        let thread = self.sessions.require(&request.session_id)?;
        thread
            .set_config_option(request.config_id, request.value)
            .await?;
        Ok(SetSessionConfigOptionResponse::new(
            thread.config_options().await?,
        ))
    }

    async fn prompt(&self, _cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        Ok(PromptResponse::new(self.run_prompt(request).await?))
    }

    async fn cancel(&self, _cx: &Cx, session: &SessionId) {
        if let Err(error) = self.interrupt(session).await {
            tracing::error!("Error handling cancel: {error:?}");
        }
    }
}
