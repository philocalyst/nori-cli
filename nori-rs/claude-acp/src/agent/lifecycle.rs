//! Creating, loading, resuming and forking sessions.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use nori_acp_kit::Cx;
use nori_acp_kit::Sink;
use nori_acp_kit::schema::ForkSessionRequest;
use nori_acp_kit::schema::ForkSessionResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionId;

use super::catalog;
use super::slash_commands::filter_commands;
use super::slash_commands::get_predefined_commands;
use crate::mcp::SessionTools;
use crate::session::PermissionMode;
use crate::session::Session;
use crate::session::SessionManager;
use crate::session::warm;
use crate::terminal::TerminalClient;
use crate::types::AgentConfig;
use crate::types::NewSessionMeta;
use crate::types::Result;

/// How long a session must go without a turn before the next session's CLI is started.
const REFILL_QUIET: Duration = Duration::from_secs(2);

/// Tell the client which slash commands exist, once the response that created the session is out.
fn announce_commands(cx: &Cx, session_id: &SessionId) {
    let sink = Sink::new(cx, session_id.clone());
    tokio::spawn(async move {
        if let Err(error) = sink.commands(filter_commands(get_predefined_commands())) {
            tracing::warn!(session_id = %sink.session_id().0, %error, "Available commands not sent");
        }
    });
}

pub(super) async fn new_session(
    request: NewSessionRequest,
    config: &AgentConfig,
    sessions: &Arc<SessionManager>,
    cx: &Cx,
) -> Result<NewSessionResponse> {
    let meta = request
        .meta
        .as_ref()
        .map(|meta| NewSessionMeta::from_request_meta(serde_json::to_value(meta).ok().as_ref()));
    let cwd = request.cwd;
    let tools = SessionTools::from_request(request.mcp_servers, request.meta.as_ref());

    // A plain new session takes one whose CLI is already running, if one fits; then the next is
    // started for whoever comes after.
    let key = warm::WarmKey::new(&cwd, config, &tools);
    let plain = meta.as_ref().is_none_or(NewSessionMeta::is_plain)
        && tools
            .host_tools
            .as_ref()
            .is_none_or(|tools| tools.matches_prewarm_catalog());
    let session = match plain.then(|| warm::claim(&key)).flatten() {
        Some(session) => {
            session.adopt_tools(&tools);
            sessions.adopt(Arc::clone(&session))?;
            tracing::info!(session_id = %session.session_id, "Took a warm Claude session");
            session
        }
        None => sessions.create_session(
            uuid::Uuid::new_v4().to_string(),
            cwd.clone(),
            config,
            meta.as_ref(),
            &tools,
        )?,
    };
    let session_id = SessionId::new(session.session_id.clone());

    // Start the Claude CLI now, while the user is still typing, so the first prompt does not pay
    // for it. A failure here is not reported: the first prompt tries again and reports it. The
    // next session's CLI starts once this one is up and quiet, so the two never compete.
    {
        let session = Arc::clone(&session);
        let cx = cx.clone();
        let next = plain.then(|| (key, cwd, config.clone(), tools.mcp_servers.clone()));
        let client_terminal = sessions.client_terminal();
        tokio::spawn(async move {
            if let Err(error) = prepare_session(&session, &cx, client_terminal).await {
                tracing::debug!(session_id = %session.session_id, %error, "Warm-up did not finish");
            }
            if let Some((key, cwd, config, mcp_servers)) = next {
                session.wait_until_idle(REFILL_QUIET).await;
                warm::refill(key, cwd, config, mcp_servers);
            }
        });
    }
    announce_commands(cx, &session_id);

    Ok(NewSessionResponse::new(session_id)
        .modes(catalog::mode_state(
            PermissionMode::BypassPermissions.as_str(),
        ))
        .config_options(catalog::session_options(&session).await))
}

pub(super) async fn load_session(
    request: LoadSessionRequest,
    config: &AgentConfig,
    sessions: &Arc<SessionManager>,
    cx: &Cx,
) -> Result<LoadSessionResponse> {
    // The loaded session keeps the id it was asked for, so the client can carry on with it.
    let id = request.session_id.0.to_string();
    let session = match sessions.get_session(&id) {
        Some(session) => session,
        None => {
            let tools = SessionTools::from_request(request.mcp_servers, request.meta.as_ref());
            sessions.create_session(
                id,
                request.cwd,
                config,
                Some(&NewSessionMeta::with_resume(request.session_id.0.as_ref())),
                &tools,
            )?
        }
    };
    announce_commands(cx, &request.session_id);
    Ok(LoadSessionResponse::new()
        .modes(catalog::mode_state(
            PermissionMode::BypassPermissions.as_str(),
        ))
        .config_options(catalog::session_options(&session).await))
}

pub(super) async fn resume_session(
    request: ResumeSessionRequest,
    config: &AgentConfig,
    sessions: &Arc<SessionManager>,
    cx: &Cx,
) -> Result<ResumeSessionResponse> {
    // The response carries no session id, so the client keeps using the one it asked for: the
    // resumed session must be found under it. Claude Code keeps the id when it resumes.
    let id = request.session_id.0.to_string();
    let session = match sessions.get_session(&id) {
        Some(session) => session,
        None => {
            let tools = SessionTools::from_request(request.mcp_servers, request.meta.as_ref());
            sessions.create_session(
                id,
                request.cwd,
                config,
                Some(&NewSessionMeta::with_resume(request.session_id.0.as_ref())),
                &tools,
            )?
        }
    };
    announce_commands(cx, &request.session_id);
    Ok(ResumeSessionResponse::new()
        .modes(catalog::mode_state(
            PermissionMode::BypassPermissions.as_str(),
        ))
        .config_options(catalog::session_options(&session).await))
}

pub(super) async fn fork_session(
    request: ForkSessionRequest,
    config: &AgentConfig,
    sessions: &Arc<SessionManager>,
    cx: &Cx,
) -> Result<ForkSessionResponse> {
    let new_id = uuid::Uuid::new_v4().to_string();
    let tools = SessionTools::from_request(request.mcp_servers, request.meta.as_ref());
    let session = sessions.create_session(
        new_id.clone(),
        request.cwd,
        config,
        Some(&NewSessionMeta::with_resume_and_fork(
            request.session_id.0.as_ref(),
        )),
        &tools,
    )?;
    let new_id = SessionId::new(new_id);
    announce_commands(cx, &new_id);
    Ok(ForkSessionResponse::new(new_id)
        .modes(catalog::mode_state(
            PermissionMode::BypassPermissions.as_str(),
        ))
        .config_options(catalog::session_options(&session).await))
}

/// Get a session ready to take its first query: wire up the ACP tools, connect external MCP
/// servers, and start the Claude CLI. Idempotent, and serialized per session, so the work that
/// starts when the session is created and the first prompt never do it twice.
pub(super) async fn prepare_session(
    session: &Arc<Session>,
    cx: &Cx,
    client_terminal: bool,
) -> Result<()> {
    let mut ready = session.ready().lock().await;
    if *ready {
        return Ok(());
    }
    let start = Instant::now();
    // The ACP tools (Bash and the rest) run commands in the client's terminals only when the
    // client runs terminals; otherwise they run them here and stream the output as updates.
    let terminal_client = client_terminal
        .then(|| Arc::new(TerminalClient::new(cx.clone(), session.session_id.clone())));
    session
        .configure_acp_server(cx.clone(), terminal_client)
        .await;
    // The can_use_tool callback asks the client for permission through the connection.
    session.set_connection_cx(cx.clone());

    // MCP servers need no connecting here: Claude Code connects to the client's servers itself,
    // and Nori's host tools are in this process.
    if !session.is_connected() {
        session.connect().await?;
        // The CLI says which models this account can use, and the client hears of them at once:
        // `/model` offers what the account has today, not a list Nori keeps.
        let reported = session
            .client()
            .await
            .get_server_info()
            .await
            .and_then(|info| {
                // The control response keeps its payload under `response`.
                info.pointer("/response/models")
                    .or_else(|| info.get("models"))
                    .and_then(serde_json::Value::as_array)
                    .cloned()
            });
        if let Some(models) = reported {
            session.set_reported_models(models);
            let options = catalog::session_options(session).await;
            let sink = Sink::new(cx, SessionId::new(session.session_id.clone()));
            if let Err(error) = sink.config_options(options) {
                tracing::debug!(session_id = %session.session_id, %error, "Model list not sent");
            }
        }
    }
    *ready = true;
    tracing::info!(
        session_id = %session.session_id,
        elapsed_ms = start.elapsed().as_millis(),
        "Session ready"
    );
    Ok(())
}
