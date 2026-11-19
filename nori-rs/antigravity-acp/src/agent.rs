//! The Antigravity SDK agent: its identity, and the requests the kit routes to it.

use std::path::PathBuf;
use std::sync::Arc;

use antigravity_sdk_rust::types::Step;
use nori_acp_kit::AgentError;
use nori_acp_kit::Capabilities;
use nori_acp_kit::Cx;
use nori_acp_kit::HostBridge;
use nori_acp_kit::NativeAgent;
use nori_acp_kit::Options;
use nori_acp_kit::Profile;
use nori_acp_kit::Sessions;
use nori_acp_kit::Sink;
use nori_acp_kit::acp;
use nori_acp_kit::async_trait;
use nori_acp_kit::mcp_endpoints;
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
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SetSessionConfigOptionRequest;
use nori_acp_kit::schema::SetSessionConfigOptionResponse;
use tokio::sync::Mutex;

use crate::connection::Connection;
use crate::connection::ForkSeed;
use crate::connection::StartConfig;
use crate::discovery::Backend;
use crate::models;
use crate::session::Session;
use crate::session::generate_session_id;
use crate::store::SessionStore;
use crate::translate;
use crate::turn;

/// What a new session is built from.
pub(crate) struct Start {
    pub(crate) id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) mcp_servers: Vec<McpServer>,
    pub(crate) meta: Option<Meta>,
    pub(crate) options: Options,
    /// History to use when the connection brings none of its own.
    pub(crate) fallback_history: Vec<Step>,
    pub(crate) title: Option<String>,
    pub(crate) fork: Option<ForkSeed>,
}

pub(crate) struct AntigravityAgent {
    pub(crate) backend: Backend,
    /// The model and thought level new sessions start with.
    pub(crate) model: Option<String>,
    pub(crate) thought_level: Option<String>,
    pub(crate) sessions: Sessions<Session>,
    pub(crate) store: SessionStore,
}

impl AntigravityAgent {
    /// Open the harness connection for `start` and register the session.
    pub(crate) async fn start(&self, start: Start) -> Result<Arc<Session>, AgentError> {
        let host = HostBridge::from_session_meta(start.meta.as_ref(), &start.id);
        let save_dir = self.store.session_dir(&start.id);
        let config = StartConfig {
            session_id: start.id.clone(),
            cwd: start.cwd.clone(),
            additional_directories: start.additional_directories.clone(),
            model: models::model(&start.options),
            thinking_level: models::thinking_level(&start.options),
            host: host.clone(),
            mcp_servers: mcp_endpoints(start.mcp_servers)?,
            save_dir: save_dir.clone(),
        };
        let conn = Connection::open(config, &self.backend, start.fork).await?;
        let mut history = conn.initial_history();
        if history.is_empty() {
            history = start.fallback_history;
        }
        let session = self.sessions.insert(
            SessionId::new(start.id.clone()),
            Session {
                id: start.id,
                cwd: start.cwd,
                additional_directories: start.additional_directories,
                options: Mutex::new(start.options),
                host,
                conn,
                history: Mutex::new(history),
                title: Mutex::new(start.title),
                save_dir,
                gate: nori_acp_kit::TurnGate::default(),
            },
        );
        session.persist(&self.store).await;
        Ok(session)
    }
}

#[async_trait]
impl NativeAgent for AntigravityAgent {
    fn profile(&self) -> Profile {
        Profile::new(
            "nori-antigravity-sdk-acp",
            "Antigravity (SDK)",
            env!("CARGO_PKG_VERSION"),
        )
        .capabilities(Capabilities {
            image: true,
            embedded_context: true,
            mcp_http: true,
            mcp_sse: true,
            load: true,
            resume: true,
            fork: true,
            list: true,
            close: true,
            delete: true,
            additional_directories: true,
            host_tools: true,
            ..Capabilities::default()
        })
    }

    async fn new_session(
        &self,
        _cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let id = generate_session_id();
        let options = models::options(self.model.as_deref(), self.thought_level.as_deref());
        let session = self
            .start(Start {
                id: id.clone(),
                cwd: request.cwd,
                additional_directories: request.additional_directories,
                mcp_servers: request.mcp_servers,
                meta: request.meta,
                options: options.clone(),
                fallback_history: Vec::new(),
                title: None,
                fork: None,
            })
            .await?;
        drop(session);
        Ok(NewSessionResponse::new(id).config_options(options.to_acp()))
    }

    async fn load_session(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        let (session, options) = self
            .attach_existing(
                &request.session_id,
                request.cwd,
                request.additional_directories,
                request.mcp_servers,
                request.meta,
            )
            .await?;
        let sink = Sink::new(cx, request.session_id);
        for step in session.history.lock().await.iter() {
            for update in translate::replay_step_updates(step, session.host.as_ref()) {
                sink.update(update)?;
            }
        }
        Ok(LoadSessionResponse::new().config_options(options))
    }

    async fn resume_session(
        &self,
        _cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let (_session, options) = self
            .attach_existing(
                &request.session_id,
                request.cwd,
                request.additional_directories,
                request.mcp_servers,
                request.meta,
            )
            .await?;
        Ok(ResumeSessionResponse::new().config_options(options))
    }

    async fn fork_session(
        &self,
        _cx: &Cx,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        Ok(self.fork(request).await?)
    }

    async fn list_sessions(
        &self,
        _cx: &Cx,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        Ok(self.list(&request)?)
    }

    async fn close_session(
        &self,
        _cx: &Cx,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        if let Some(session) = self.sessions.remove(&request.session_id) {
            session.stop().await;
        }
        Ok(CloseSessionResponse::new())
    }

    async fn delete_session(
        &self,
        _cx: &Cx,
        request: DeleteSessionRequest,
    ) -> acp::Result<DeleteSessionResponse> {
        if let Some(session) = self.sessions.remove(&request.session_id) {
            session.stop().await;
        }
        self.store
            .delete(&request.session_id.to_string())
            .map_err(AgentError::from)?;
        Ok(DeleteSessionResponse::new())
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let session = self.sessions.require(&request.session_id)?;
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| AgentError::invalid("option values are selections"))?
            .to_string();
        let (options, model, level) = {
            let mut options = session.options.lock().await;
            let updated = options.set(&request.config_id.to_string(), &value)?;
            (
                updated,
                models::model(&options),
                models::thinking_level(&options),
            )
        };
        session.conn.apply_config(&model, level).await;
        session.persist(&self.store).await;
        Ok(SetSessionConfigOptionResponse::new(options))
    }

    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        let session = self.sessions.require(&request.session_id)?;
        let running = session.gate.begin().await?;
        let sink = Sink::new(cx, request.session_id);
        Ok(turn::run(&session, &self.store, sink, running, request.prompt).await?)
    }

    async fn cancel(&self, _cx: &Cx, session: &SessionId) {
        if let Some(session) = self.sessions.get(session)
            && session.gate.cancel()
        {
            session.conn.halt().await;
        }
    }

    async fn shutdown(&self) {
        for session in self.sessions.drain() {
            session.stop().await;
        }
    }
}
