//! Session attachment, listing, forking and cleanup for the OpenCode v2 adapter.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use nori_acp_kit::AgentError;
use nori_acp_kit::Cx;
use nori_acp_kit::TurnGate;
use nori_acp_kit::acp;
use nori_acp_kit::schema::ForkSessionRequest;
use nori_acp_kit::schema::ForkSessionResponse;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionInfo;
use reqwest::Method;
use serde_json::json;
use tokio::sync::Mutex;

use crate::OpenCodeServer;
use crate::agent::OpenCodeAgent;
use crate::agent::Session;
use crate::config::Catalog;
use crate::server::failure;

impl Session {
    pub(crate) fn new(server: Arc<OpenCodeServer>, catalog: Catalog) -> Self {
        Self {
            server,
            catalog: Mutex::new(catalog),
            gate: TurnGate::default(),
            cancelled: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            native_child: false,
            permission_reply: Mutex::new(()),
            mcp: None,
            owns_mcp: false,
        }
    }
}

impl OpenCodeAgent {
    pub(crate) async fn resolve_session(&self, id: &str) -> acp::Result<Arc<Session>> {
        if let Some(session) = self.sessions.get(&SessionId::new(id)) {
            return Ok(session);
        }
        let parent = self
            .known_children
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| AgentError::unknown_session(id))?;
        if parent.closed.load(Ordering::SeqCst) {
            return Err(AgentError::unknown_session(id).into());
        }
        let existing = parent
            .server
            .request(Method::GET, &["session", id], None)
            .await
            .map_err(failure)?;
        let catalog = Catalog::load(&parent.server, &existing["data"]).await?;
        let mut session = Session::new(parent.server.clone(), catalog);
        session.native_child = true;
        session.mcp = parent.mcp.clone();
        let id = SessionId::new(id);
        Ok(self
            .sessions
            .get(&id)
            .unwrap_or_else(|| self.sessions.insert(id, session)))
    }

    async fn attach(
        &self,
        id: &str,
        cwd: &Path,
        additional: Option<Vec<PathBuf>>,
        mcp_servers: Vec<McpServer>,
        register_mcp: bool,
    ) -> acp::Result<(Session, Vec<SessionConfigOption>)> {
        if !cwd.is_absolute() {
            return Err(acp::Error::invalid_params().data("OpenCode requires an absolute cwd"));
        }
        let server = OpenCodeServer::start(cwd, &self.environment)
            .await
            .map_err(failure)?;
        let existing = server
            .request(Method::GET, &["session", id], None)
            .await
            .map_err(failure)?;
        let actual = existing["data"]["location"]["directory"]
            .as_str()
            .ok_or_else(|| {
                acp::Error::internal_error().data("OpenCode session has no directory")
            })?;
        if std::fs::canonicalize(cwd).map_err(acp::Error::into_internal_error)?
            != std::fs::canonicalize(actual).map_err(acp::Error::into_internal_error)?
        {
            return Err(acp::Error::invalid_params().data("Session directory does not match cwd"));
        }
        if let Some(additional) = additional {
            let grant = crate::directories::update(cwd, &existing["data"], additional)?;
            server
                .request(Method::PATCH, &["session", id], Some(&grant))
                .await
                .map_err(failure)?;
        }
        let mcp = if register_mcp {
            Some(
                crate::session_mcp::SessionMcp::create(
                    &server,
                    id.to_owned(),
                    cwd.to_path_buf(),
                    mcp_servers,
                )
                .await?,
            )
        } else {
            None
        };
        let catalog = Catalog::load(&server, &existing["data"]).await?;
        let options = catalog.options()?;
        let mut session = Session::new(Arc::new(server), catalog);
        session.native_child = existing["data"]["parentID"].is_string();
        session.owns_mcp = mcp.is_some();
        session.mcp = mcp;
        Ok((session, options))
    }

    pub(crate) async fn load(
        &self,
        request: LoadSessionRequest,
        client: &Cx,
    ) -> acp::Result<LoadSessionResponse> {
        let id = request.session_id.to_string();
        let (session, options) = self
            .attach(
                &id,
                &request.cwd,
                Some(request.additional_directories),
                request.mcp_servers,
                true,
            )
            .await?;
        crate::replay::history(&session.server, client, &id, &request.cwd).await?;
        self.sessions.insert(request.session_id, session);
        Ok(LoadSessionResponse::new().config_options(options))
    }

    pub(crate) async fn resume(
        &self,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        let id = request.session_id.to_string();
        let (session, options) = self
            .attach(
                &id,
                &request.cwd,
                Some(request.additional_directories),
                request.mcp_servers,
                true,
            )
            .await?;
        self.sessions.insert(request.session_id, session);
        Ok(ResumeSessionResponse::new().config_options(options))
    }

    pub(crate) async fn fork(
        &self,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        let source_id = request.session_id.to_string();
        let (session, _) = self
            .attach(&source_id, &request.cwd, None, Vec::new(), false)
            .await?;
        let forked = session
            .server
            .request(
                Method::POST,
                &["session", &source_id, "fork"],
                Some(&json!({})),
            )
            .await
            .map_err(failure)?;
        let id = forked["data"]["id"]
            .as_str()
            .ok_or_else(|| acp::Error::internal_error().data("OpenCode fork has no session id"))?
            .to_owned();
        let grant = crate::directories::update(
            &request.cwd,
            &forked["data"],
            request.additional_directories,
        )?;
        session
            .server
            .request(Method::PATCH, &["session", &id], Some(&grant))
            .await
            .map_err(failure)?;
        let catalog = Catalog::load(&session.server, &forked["data"]).await?;
        let options = catalog.options()?;
        let mcp = crate::session_mcp::SessionMcp::create(
            &session.server,
            id.clone(),
            request.cwd,
            request.mcp_servers,
        )
        .await?;
        let mut fork_session = Session::new(session.server, catalog);
        fork_session.mcp = Some(mcp);
        fork_session.owns_mcp = true;
        self.sessions
            .insert(SessionId::new(id.clone()), fork_session);
        Ok(ForkSessionResponse::new(id).config_options(options))
    }

    pub(crate) async fn list(
        &self,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        if request.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return Err(acp::Error::invalid_params().data("cwd must be absolute"));
        }
        let cwd = std::fs::canonicalize(
            request
                .cwd
                .clone()
                .unwrap_or(std::env::current_dir().map_err(acp::Error::into_internal_error)?),
        )
        .map_err(acp::Error::into_internal_error)?;
        let server = OpenCodeServer::start(&cwd, &self.environment)
            .await
            .map_err(failure)?;
        let mut query = vec![("order", "desc"), ("limit", "100")];
        if request.cwd.is_some() {
            query.push((
                "directory",
                cwd.to_str().ok_or_else(acp::Error::invalid_params)?,
            ));
        }
        if let Some(cursor) = request.cursor.as_deref() {
            query.push(("cursor", cursor));
        }
        let page = server
            .request_with_query(Method::GET, &["session"], &query, None)
            .await
            .map_err(failure)?;
        let sessions = page["data"]
            .as_array()
            .ok_or_else(|| acp::Error::internal_error().data("Invalid OpenCode session list"))?
            .iter()
            .filter_map(|session| {
                let id = session["id"].as_str()?;
                let cwd = session["location"]["directory"].as_str()?;
                let additional = session["metadata"]["opencode.acp.additionalDirectories"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|path| path.as_str().map(PathBuf::from))
                    .collect();
                Some(
                    SessionInfo::new(id.to_owned(), cwd)
                        .title(session["title"].as_str().map(ToOwned::to_owned))
                        .additional_directories(additional),
                )
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions)
            .next_cursor(page["cursor"]["next"].as_str().map(ToOwned::to_owned)))
    }

    /// Forget the session `id` and everything it started, leaving only the server's own copy.
    async fn retire(&self, id: &SessionId) -> Option<Arc<Session>> {
        self.known_children.lock().await.remove(&id.to_string());
        let session = self.sessions.remove(id)?;
        session.closed.store(true, Ordering::SeqCst);
        session.cancelled.store(true, Ordering::SeqCst);
        session.gate.cancel();
        if session.owns_mcp
            && let Some(mcp) = &session.mcp
        {
            crate::session_mcp::SessionMcp::close(mcp, &session.server).await;
        }
        self.known_children
            .lock()
            .await
            .retain(|_, parent| !Arc::ptr_eq(parent, &session));
        Some(session)
    }

    pub(crate) async fn close(&self, id: &SessionId) -> acp::Result<()> {
        if let Some(session) = self.retire(id).await {
            let _permission_reply = session.permission_reply.lock().await;
            session
                .server
                .request(
                    Method::POST,
                    &["session", &id.to_string(), "interrupt"],
                    Some(&json!({})),
                )
                .await
                .map_err(failure)?;
        }
        Ok(())
    }

    pub(crate) async fn delete(&self, id: &SessionId) -> acp::Result<()> {
        let path = id.to_string();
        if let Some(session) = self.retire(id).await {
            let _permission_reply = session.permission_reply.lock().await;
            session
                .server
                .request(Method::DELETE, &["session", &path], None)
                .await
                .map_err(failure)?;
            return Ok(());
        }
        let cwd = std::env::current_dir().map_err(acp::Error::into_internal_error)?;
        let server = OpenCodeServer::start(&cwd, &self.environment)
            .await
            .map_err(failure)?;
        if let Err(error) = server
            .request(Method::DELETE, &["session", &path], None)
            .await
            && !error
                .downcast_ref::<reqwest::Error>()
                .is_some_and(|error| error.status() == Some(reqwest::StatusCode::NOT_FOUND))
        {
            return Err(failure(error).into());
        }
        Ok(())
    }
}
