//! The OpenCode adapter as a kit [`NativeAgent`]: its sessions, and the requests routed to them.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nori_acp_kit::AgentError;
use nori_acp_kit::Capabilities;
use nori_acp_kit::Cx;
use nori_acp_kit::InProcessSpec;
use nori_acp_kit::NativeAgent;
use nori_acp_kit::Profile;
use nori_acp_kit::Registration;
use nori_acp_kit::Sessions;
use nori_acp_kit::Sink;
use nori_acp_kit::SpecOptions;
use nori_acp_kit::TurnGate;
use nori_acp_kit::acp;
use nori_acp_kit::async_trait;
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
use reqwest::Method;
use serde_json::json;
use tokio::sync::Mutex;

use crate::OpenCodeServer;
use crate::server::failure;

/// Offer the OpenCode adapter to the host as the native agent `opencode`.
pub fn registration() -> Registration {
    Registration::new(
        "opencode",
        |spec| async move { OpenCodeAgent::from_spec(spec) },
    )
}

pub(crate) struct OpenCodeAgent {
    pub(crate) environment: HashMap<OsString, OsString>,
    pub(crate) sessions: Sessions<Session>,
    /// Native children (subagent sessions) by id, each with the session whose turn spawned it.
    pub(crate) known_children: Arc<Mutex<HashMap<String, Arc<Session>>>>,
}

pub(crate) struct Session {
    pub(crate) server: Arc<OpenCodeServer>,
    pub(crate) gate: TurnGate,
    /// Set by a cancel, cleared by the next prompt. Outlives the turn, so a permission or
    /// question answered after the cancel is still refused.
    pub(crate) cancelled: AtomicBool,
    pub(crate) closed: AtomicBool,
    pub(crate) native_child: bool,
    pub(crate) permission_reply: Mutex<()>,
    pub(crate) catalog: Mutex<crate::config::Catalog>,
    pub(crate) mcp: Option<crate::session_mcp::SharedSessionMcp>,
    pub(crate) owns_mcp: bool,
}

impl OpenCodeAgent {
    fn from_spec(spec: InProcessSpec) -> std::io::Result<Self> {
        // `cli_path` is accepted and ignored: the adapter talks to a running server and never
        // starts `opencode`, but existing configurations still carry the key.
        let options = SpecOptions::parse(
            "OpenCode",
            &spec.config,
            &["server_url", "server_password", "cli_path"],
        )?;
        let mut environment = spec.environment.clone();
        if let Some(password) = options.get("server_password") {
            environment.insert("OPENCODE_PASSWORD".to_owned(), password.to_owned());
        }
        if let Some(home) = &spec.home {
            for (key, directory) in [
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_CACHE_HOME", "cache"),
            ] {
                environment
                    .entry(key.to_owned())
                    .or_insert_with(|| home.join(directory).to_string_lossy().into_owned());
            }
        }
        if let Some(url) = options.get("server_url") {
            environment.insert("NORI_OPENCODE_URL".to_owned(), url.to_owned());
        }
        Ok(Self::new(
            environment
                .into_iter()
                .map(|(key, value)| (key.into(), value.into()))
                .collect(),
        ))
    }

    pub(crate) fn new(environment: HashMap<OsString, OsString>) -> Self {
        Self {
            environment,
            sessions: Sessions::default(),
            known_children: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Tell the client about the session's slash commands.
    async fn send_commands(&self, id: &SessionId, cx: &Cx) -> acp::Result<()> {
        let session = self.sessions.require(id)?;
        let commands = session.catalog.lock().await.commands().to_vec();
        let commands = commands
            .iter()
            .filter_map(|command| {
                Some(AvailableCommand::new(
                    command["name"].as_str()?.to_owned(),
                    command["description"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                ))
            })
            .collect();
        Sink::new(cx, id.clone()).commands(commands)
    }

    pub(crate) async fn create(
        &self,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        let mut grant = crate::directories::update(
            &request.cwd,
            &serde_json::Value::Null,
            request.additional_directories,
        )?;
        let cwd = std::fs::canonicalize(&request.cwd).map_err(acp::Error::into_internal_error)?;
        grant["location"] = json!({"directory": cwd});
        let server = OpenCodeServer::start(&request.cwd, &self.environment)
            .await
            .map_err(failure)?;
        let result = server
            .request(Method::POST, &["session"], Some(&grant))
            .await
            .map_err(failure)?;
        let actual_cwd = result["data"]["location"]["directory"]
            .as_str()
            .ok_or_else(|| {
                AgentError::internal("OpenCode created a session without a directory")
            })?;
        if std::fs::canonicalize(actual_cwd).map_err(acp::Error::into_internal_error)? != cwd {
            return Err(AgentError::internal(
                "OpenCode created the session in a different directory",
            )
            .into());
        }
        let session_id = result["data"]["id"]
            .as_str()
            .ok_or_else(|| AgentError::internal("OpenCode returned a session without an id"))?
            .to_owned();
        let mcp = crate::session_mcp::SessionMcp::create(
            &server,
            session_id.clone(),
            request.cwd,
            request.mcp_servers,
        )
        .await?;
        let catalog = crate::config::Catalog::load(&server, &result["data"]).await?;
        let options = catalog.options()?;
        let mut session = Session::new(Arc::new(server), catalog);
        session.mcp = Some(mcp);
        session.owns_mcp = true;
        self.sessions
            .insert(SessionId::new(session_id.clone()), session);
        Ok(NewSessionResponse::new(session_id).config_options(options))
    }

    /// Reply to a request for the config option `config` of session `id`.
    async fn configure(
        &self,
        id: &SessionId,
        config: &str,
        value: &str,
    ) -> acp::Result<Vec<nori_acp_kit::schema::SessionConfigOption>> {
        let session = self.resolve_session(id.0.as_ref()).await?;
        session
            .catalog
            .lock()
            .await
            .set(&session.server, &id.to_string(), config, value)
            .await
    }
}

#[async_trait]
impl NativeAgent for OpenCodeAgent {
    fn profile(&self) -> Profile {
        Profile::new("nori-opencode-acp", "OpenCode", env!("CARGO_PKG_VERSION")).capabilities(
            Capabilities {
                image: true,
                embedded_context: true,
                mcp_http: true,
                load: true,
                resume: true,
                fork: true,
                list: true,
                close: true,
                delete: true,
                additional_directories: true,
                ..Capabilities::default()
            },
        )
    }

    async fn new_session(
        &self,
        _cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        self.create(request).await
    }

    async fn load_session(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        self.load(request, cx).await
    }

    async fn resume_session(
        &self,
        _cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        self.resume(request).await
    }

    async fn fork_session(
        &self,
        _cx: &Cx,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        self.fork(request).await
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
        self.close(&request.session_id).await?;
        Ok(CloseSessionResponse::new())
    }

    async fn delete_session(
        &self,
        _cx: &Cx,
        request: DeleteSessionRequest,
    ) -> acp::Result<DeleteSessionResponse> {
        self.delete(&request.session_id).await?;
        Ok(DeleteSessionResponse::new())
    }

    async fn set_session_mode(
        &self,
        cx: &Cx,
        request: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        let options = self
            .configure(&request.session_id, "mode", &request.mode_id.to_string())
            .await?;
        Sink::new(cx, request.session_id).config_options(options)?;
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| AgentError::invalid("option values are selections"))?
            .to_string();
        let options = self
            .configure(&request.session_id, &request.config_id.to_string(), &value)
            .await?;
        Ok(SetSessionConfigOptionResponse::new(options))
    }

    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        let session = self.resolve_session(request.session_id.0.as_ref()).await?;
        let turn = session.gate.begin().await?;
        session.cancelled.store(false, Ordering::SeqCst);
        crate::turn::run(
            session,
            request,
            cx.clone(),
            self.known_children.clone(),
            turn,
        )
        .await
    }

    async fn cancel(&self, _cx: &Cx, session_id: &SessionId) {
        let id = session_id.to_string();
        let Ok(session) = self.resolve_session(&id).await else {
            return;
        };
        let running = session.gate.is_running();
        if !session.native_child && !running {
            return;
        }
        let _permission_reply = session.permission_reply.lock().await;
        session.cancelled.store(true, Ordering::SeqCst);
        session.gate.cancel();
        let _ = session
            .server
            .request(
                Method::POST,
                &["session", &id, "interrupt"],
                Some(&json!({})),
            )
            .await;
    }

    async fn session_ready(&self, cx: &Cx, session: &SessionId) {
        if let Err(error) = self.send_commands(session, cx).await {
            tracing::warn!(%error, "Failed to send OpenCode slash commands");
        }
    }

    async fn shutdown(&self) {
        for session in self.sessions.drain() {
            session.closed.store(true, Ordering::SeqCst);
            if session.owns_mcp
                && let Some(mcp) = &session.mcp
            {
                crate::session_mcp::SessionMcp::close(mcp, &session.server).await;
            }
        }
    }
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
