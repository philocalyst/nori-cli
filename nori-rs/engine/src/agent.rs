//! A [`Flavor`] served as an ACP agent: the session table and every request.

use std::path::PathBuf;
use std::sync::Arc;

use nori_acp_kit::AgentError;
use nori_acp_kit::Capabilities;
use nori_acp_kit::Cx;
use nori_acp_kit::HostBridge;
use nori_acp_kit::NativeAgent;
use nori_acp_kit::Profile;
use nori_acp_kit::Sessions;
use nori_acp_kit::Sink;
use nori_acp_kit::acp;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::AuthenticateRequest;
use nori_acp_kit::schema::AuthenticateResponse;
use nori_acp_kit::schema::AvailableCommand;
use nori_acp_kit::schema::CloseSessionRequest;
use nori_acp_kit::schema::CloseSessionResponse;
use nori_acp_kit::schema::DeleteSessionRequest;
use nori_acp_kit::schema::DeleteSessionResponse;
use nori_acp_kit::schema::ForkSessionRequest;
use nori_acp_kit::schema::ForkSessionResponse;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::LogoutRequest;
use nori_acp_kit::schema::LogoutResponse;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionInfo;
use nori_acp_kit::schema::SetSessionConfigOptionRequest;
use nori_acp_kit::schema::SetSessionConfigOptionResponse;
use nori_acp_kit::schema::SetSessionModeRequest;
use nori_acp_kit::schema::SetSessionModeResponse;
use nori_acp_kit::schema::StopReason;

use crate::flavor::Flavor;
use crate::options::Dials;
use crate::session::Parts;
use crate::session::Session;
use crate::store::Journal;
use crate::store::Recorded;
use crate::store::SessionMeta;
use crate::store::Store;
use crate::turn::TurnEnv;

/// What a session is opened from.
struct Opening<'a> {
    id: &'a str,
    mcp_servers: Vec<nori_acp_kit::schema::McpServer>,
    cwd: PathBuf,
    additional: Vec<PathBuf>,
    meta: Option<&'a Meta>,
    recorded: Option<&'a Recorded>,
    journal: Journal,
}

/// An engine agent: `F`'s models and tools behind Nori's loop.
pub struct EngineAgent<F: Flavor> {
    flavor: Arc<F>,
    store: Store,
    client: nori_llm::Client,
    sessions: Sessions<Session>,
    /// Whether the client is Nori's own harness, which alone knows Nori's extensions.
    nori_host: std::sync::atomic::AtomicBool,
}

impl<F: Flavor> EngineAgent<F> {
    pub fn new(flavor: F) -> Self {
        let store = Store::new(&flavor.store_root());
        Self {
            flavor: Arc::new(flavor),
            store,
            client: nori_llm::Client::new(),
            sessions: Sessions::default(),
            nori_host: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn flavor(&self) -> &F {
        &self.flavor
    }

    /// Build the live session for a journal.
    async fn open(
        &self,
        cx: &Cx,
        opening: Opening<'_>,
    ) -> Result<(Arc<Session>, Vec<SessionConfigOption>), AgentError> {
        let Opening {
            id,
            mcp_servers,
            cwd,
            additional,
            meta,
            recorded,
            journal,
        } = opening;
        let workspace = Arc::new(self.flavor.workspace(&cwd).await?);
        let config = recorded
            .map(|recorded| recorded.config.clone())
            .unwrap_or_default();
        let dials = Dials::initial(&workspace, &config)?;
        if recorded.is_none_or(|recorded| recorded.config != dials.config()) {
            journal.config(&dials.config()).await?;
        }
        let options = dials.options(&workspace);
        let mut roots = vec![cwd.clone()];
        roots.extend(additional);
        let mut servers = mcp_servers;
        for server in &workspace.mcp_servers {
            let name = server_name(server);
            if !servers.iter().any(|known| server_name(known) == name) {
                servers.push(server.clone());
            }
        }
        let session = self.sessions.insert(
            SessionId::new(id.to_owned()),
            Session::new(Parts {
                id: id.to_owned(),
                cwd,
                roots,
                workspace,
                dials,
                journal,
                host: HostBridge::from_session_meta(meta, id),
                history: recorded
                    .map(|recorded| recorded.messages.clone())
                    .unwrap_or_default(),
                flavor_tools: self.flavor.tools(),
                files: self.flavor.file_tools(),
                mcp_servers: servers,
            }),
        );
        self.follow_workspace(cx, &session);
        self.announce_auth(cx);
        Ok((session, options))
    }

    /// Offer `session` the flavor's workspace again each time it changes, for as long as the
    /// session is open, and tell the client what it now offers.
    fn follow_workspace(&self, cx: &Cx, session: &Arc<Session>) {
        let Some(mut changes) = self.flavor.workspace_changes() else {
            return;
        };
        let (flavor, cx, open) = (
            Arc::clone(&self.flavor),
            cx.clone(),
            Arc::downgrade(session),
        );
        tokio::spawn(async move {
            while changes.changed().await.is_ok() {
                let Some(session) = open.upgrade() else {
                    return;
                };
                let workspace = match flavor.workspace(&session.cwd).await {
                    Ok(workspace) => Arc::new(workspace),
                    Err(error) => {
                        tracing::debug!(%error, "could not read the changed workspace");
                        continue;
                    }
                };
                let options = session
                    .replace_workspace(Arc::clone(&workspace))
                    .options(&workspace);
                let sent =
                    Sink::new(&cx, SessionId::new(session.id.clone())).config_options(options);
                if let Err(error) = sent {
                    tracing::debug!(%error, "could not offer the changed workspace");
                }
            }
        });
    }

    /// Tell Nori who pays for the agent's turns, off the request's path.
    fn announce_auth(&self, cx: &Cx) {
        if !self.nori_host.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let flavor = Arc::clone(&self.flavor);
        let cx = cx.clone();
        tokio::spawn(async move {
            let status = flavor.auth_status().await;
            if let Err(error) = nori_acp_kit::announce_auth_status(&cx, &status) {
                tracing::debug!(%error, "could not announce the sign-in");
            }
        });
    }

    fn require(&self, id: &SessionId) -> Result<Arc<Session>, AgentError> {
        self.sessions.require(id)
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
    }

    /// Reopen a recorded session (for load and resume).
    async fn reopen(
        &self,
        cx: &Cx,
        id: &SessionId,
        cwd: PathBuf,
        additional: Vec<PathBuf>,
        meta: Option<&Meta>,
        mcp_servers: Vec<nori_acp_kit::schema::McpServer>,
    ) -> Result<(Arc<Session>, Recorded, Vec<SessionConfigOption>), AgentError> {
        let (recorded, journal) = self.store.open(&id.to_string()).await?;
        // A session that is already open stays the one session: its journal is read for the
        // history (never its live conversation, which a running turn holds).
        if let Some(live) = self.sessions.get(id) {
            let options = live.dials().options(&live.workspace());
            return Ok((live, recorded, options));
        }
        let (session, options) = self
            .open(
                cx,
                Opening {
                    id: &id.to_string(),
                    mcp_servers,
                    cwd,
                    additional,
                    meta,
                    recorded: Some(&recorded),
                    journal,
                },
            )
            .await?;
        Ok((session, recorded, options))
    }
}

#[async_trait]
impl<F: Flavor> NativeAgent for EngineAgent<F> {
    fn on_host(&self, host: nori_acp_kit::Host) {
        self.nori_host.store(
            host == nori_acp_kit::Host::Nori,
            std::sync::atomic::Ordering::Relaxed,
        );
    }

    fn profile(&self) -> Profile {
        let identity = self.flavor.identity();
        Profile::new(identity.name, identity.title, env!("CARGO_PKG_VERSION"))
            .capabilities(Capabilities {
                image: true,
                embedded_context: true,
                load: true,
                resume: true,
                fork: true,
                list: true,
                close: true,
                mcp_http: true,
                delete: true,
                additional_directories: true,
                host_tools: true,
                logout: self.flavor.can_logout(),
                ..Capabilities::default()
            })
            .auth_methods(self.flavor.auth_methods())
    }

    async fn authenticate(
        &self,
        cx: &Cx,
        request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        self.flavor
            .authenticate(&request.method_id.to_string())
            .await?;
        self.announce_auth(cx);
        Ok(AuthenticateResponse::new())
    }

    async fn logout(&self, cx: &Cx, _request: LogoutRequest) -> acp::Result<LogoutResponse> {
        self.flavor.logout().await?;
        self.announce_auth(cx);
        Ok(LogoutResponse::new())
    }

    async fn new_session(
        &self,
        cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let id = uuid::Uuid::new_v4().to_string();
        let journal = self
            .store
            .create(SessionMeta {
                id: id.clone(),
                cwd: request.cwd.clone(),
                created: Self::now(),
                forked_from: None,
            })
            .await?;
        let (_, options) = self
            .open(
                cx,
                Opening {
                    id: &id,
                    mcp_servers: request.mcp_servers,
                    cwd: request.cwd,
                    additional: request.additional_directories,
                    meta: request.meta.as_ref(),
                    recorded: None,
                    journal,
                },
            )
            .await?;
        Ok(NewSessionResponse::new(id).config_options(options))
    }

    async fn load_session(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        let (_, recorded, options) = self
            .reopen(
                cx,
                &request.session_id,
                request.cwd,
                request.additional_directories,
                request.meta.as_ref(),
                request.mcp_servers,
            )
            .await?;
        let sink = Sink::new(cx, request.session_id.clone());
        crate::replay::replay(&sink, &recorded.messages)?;
        Ok(LoadSessionResponse::new().config_options(options))
    }

    async fn resume_session(
        &self,
        cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let (_, _, options) = self
            .reopen(
                cx,
                &request.session_id,
                request.cwd,
                request.additional_directories,
                request.meta.as_ref(),
                request.mcp_servers,
            )
            .await?;
        Ok(ResumeSessionResponse::new().config_options(options))
    }

    async fn fork_session(
        &self,
        cx: &Cx,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        let (source, _) = self.store.open(&request.session_id.to_string()).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let meta = SessionMeta {
            id: id.clone(),
            cwd: request.cwd.clone(),
            created: Self::now(),
            forked_from: Some(request.session_id.to_string()),
        };
        let journal = self.store.fork(&source, meta).await?;
        let (_, options) = self
            .open(
                cx,
                Opening {
                    id: &id,
                    mcp_servers: request.mcp_servers,
                    cwd: request.cwd,
                    additional: request.additional_directories,
                    meta: request.meta.as_ref(),
                    recorded: Some(&source),
                    journal,
                },
            )
            .await?;
        Ok(ForkSessionResponse::new(id).config_options(options))
    }

    async fn list_sessions(
        &self,
        _cx: &Cx,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        let sessions = self
            .store
            .list()
            .await?
            .into_iter()
            .filter(|listed| {
                request
                    .cwd
                    .as_ref()
                    .is_none_or(|cwd| &listed.meta.cwd == cwd)
            })
            .map(|listed| {
                let updated = listed
                    .updated
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0));
                SessionInfo::new(listed.meta.id, listed.meta.cwd)
                    .title(listed.title)
                    .updated_at(nori_acp_kit::rfc3339_utc(updated))
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions))
    }

    async fn close_session(
        &self,
        _cx: &Cx,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        if let Some(session) = self.sessions.remove(&request.session_id) {
            session.gate.cancel();
            session.journal.sync().await;
        }
        Ok(CloseSessionResponse::new())
    }

    async fn delete_session(
        &self,
        _cx: &Cx,
        request: DeleteSessionRequest,
    ) -> acp::Result<DeleteSessionResponse> {
        if let Some(session) = self.sessions.remove(&request.session_id) {
            session.gate.cancel();
        }
        self.store.delete(&request.session_id.to_string()).await?;
        Ok(DeleteSessionResponse::new())
    }

    async fn set_session_mode(
        &self,
        cx: &Cx,
        request: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        let session = self.require(&request.session_id)?;
        let dials = session.set_dial(nori_acp_kit::MODE, &request.mode_id.to_string())?;
        record_dials(&session, &dials);
        Sink::new(cx, request.session_id).config_options(dials.options(&session.workspace()))?;
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let session = self.require(&request.session_id)?;
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| AgentError::invalid("option values are selections"))?
            .to_string();
        let dials = session.set_dial(&request.config_id.to_string(), &value)?;
        record_dials(&session, &dials);
        Ok(SetSessionConfigOptionResponse::new(
            dials.options(&session.workspace()),
        ))
    }

    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        let session = self.require(&request.session_id)?;
        let turn = session.gate.begin().await?;
        let message =
            crate::prompt::user_message(request.prompt, &session.workspace().commands).await?;
        let ticket = session.enqueue(message);
        let sink = Sink::new(cx, request.session_id.clone());
        let env = TurnEnv {
            flavor: self.flavor.as_ref(),
            client: &self.client,
            session: &session,
            sink: &sink,
            cancel: turn.token(),
        };
        let stop = crate::turn::run(&env, ticket).await?;
        Ok(PromptResponse::new(if turn.is_cancelled() {
            StopReason::Cancelled
        } else {
            stop
        }))
    }

    async fn cancel(&self, _cx: &Cx, session: &SessionId) {
        if let Some(session) = self.sessions.get(session) {
            session.gate.cancel();
        }
    }

    async fn session_ready(&self, cx: &Cx, id: &SessionId) {
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        let commands = session
            .workspace()
            .commands
            .iter()
            .map(|command| AvailableCommand::new(command.name.clone(), command.description.clone()))
            .collect();
        if let Err(error) = Sink::new(cx, id.clone()).commands(commands) {
            tracing::debug!(%error, "could not send the slash commands");
        }
    }

    async fn shutdown(&self) {
        for session in self.sessions.drain() {
            session.gate.cancel();
            session.journal.sync().await;
        }
    }
}

/// An MCP server's name; the client's server wins over a configured one of the same name.
fn server_name(server: &nori_acp_kit::schema::McpServer) -> String {
    match server {
        nori_acp_kit::schema::McpServer::Stdio(stdio) => stdio.name.clone(),
        nori_acp_kit::schema::McpServer::Http(http) => http.name.clone(),
        nori_acp_kit::schema::McpServer::Sse(sse) => sse.name.clone(),
        _ => String::new(),
    }
}

/// Record the dials in the journal without making the answer wait for the disk.
fn record_dials(session: &Arc<Session>, dials: &Dials) {
    let journal = session.journal.clone();
    let config = dials.config();
    tokio::spawn(async move {
        if let Err(error) = journal.config(&config).await {
            tracing::warn!(%error, "could not record a configuration change");
        }
    });
}
