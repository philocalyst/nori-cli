//! Attaching to, forking and listing stored sessions.

use std::path::PathBuf;
use std::sync::Arc;

use nori_acp_kit::AgentError;
use nori_acp_kit::schema::ForkSessionRequest;
use nori_acp_kit::schema::ForkSessionResponse;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionId;

use crate::agent::AntigravityAgent;
use crate::agent::Start;
use crate::connection::ForkSeed;
use crate::models;
use crate::session::Session;
use crate::session::generate_session_id;

const PAGE_SIZE: usize = 50;

impl AntigravityAgent {
    /// The live session `id`, or the stored one brought back to life.
    pub(crate) async fn attach_existing(
        &self,
        id: &SessionId,
        cwd: PathBuf,
        additional_directories: Vec<PathBuf>,
        mcp_servers: Vec<McpServer>,
        meta: Option<Meta>,
    ) -> Result<(Arc<Session>, Vec<SessionConfigOption>), AgentError> {
        if let Some(existing) = self.sessions.get(id) {
            let options = existing.options.lock().await.to_acp();
            return Ok((existing, options));
        }
        let persisted = self
            .store
            .load(&id.to_string())
            .ok_or_else(|| AgentError::unknown_session(id))?;
        let options = models::options(Some(&persisted.model), Some(&persisted.thought_level));
        let session = self
            .start(Start {
                id: id.to_string(),
                cwd,
                additional_directories,
                mcp_servers,
                meta,
                options: options.clone(),
                fallback_history: persisted.history,
                title: persisted.title,
                fork: None,
            })
            .await?;
        Ok((session, options.to_acp()))
    }

    pub(crate) async fn fork(
        &self,
        request: ForkSessionRequest,
    ) -> Result<ForkSessionResponse, AgentError> {
        let source_id = request.session_id.to_string();
        let (options, seed, title) = if let Some(source) = self.sessions.get(&request.session_id) {
            let seed = ForkSeed {
                source_session_id: source_id,
                history: source.history.lock().await.clone(),
                source_save_dir: Some(source.save_dir.clone()),
            };
            let options = source.options.lock().await.clone();
            (options, seed, source.title.lock().await.clone())
        } else if let Some(persisted) = self.store.load(&source_id) {
            let options = models::options(Some(&persisted.model), Some(&persisted.thought_level));
            let seed = ForkSeed {
                history: persisted.history,
                source_save_dir: Some(self.store.session_dir(&source_id)),
                source_session_id: source_id,
            };
            (options, seed, persisted.title)
        } else {
            return Err(AgentError::unknown_session(&request.session_id));
        };
        let id = generate_session_id();
        self.start(Start {
            id: id.clone(),
            cwd: request.cwd,
            additional_directories: request.additional_directories,
            mcp_servers: request.mcp_servers,
            meta: request.meta,
            options: options.clone(),
            fallback_history: seed.history.clone(),
            title,
            fork: Some(seed),
        })
        .await?;
        Ok(ForkSessionResponse::new(id).config_options(options.to_acp()))
    }

    pub(crate) fn list(
        &self,
        request: &ListSessionsRequest,
    ) -> Result<ListSessionsResponse, AgentError> {
        if request.cwd.as_ref().is_some_and(|cwd| !cwd.is_absolute()) {
            return Err(AgentError::invalid("cwd must be absolute"));
        }
        let all = self.store.list(request.cwd.as_deref());
        let offset = request
            .cursor
            .as_deref()
            .and_then(|cursor| cursor.parse::<usize>().ok())
            .unwrap_or(0);
        let page: Vec<_> = all.iter().skip(offset).take(PAGE_SIZE).cloned().collect();
        let next = (offset + page.len() < all.len()).then(|| (offset + page.len()).to_string());
        Ok(ListSessionsResponse::new(page).next_cursor(next))
    }
}
