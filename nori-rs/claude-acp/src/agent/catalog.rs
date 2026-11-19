//! The modes and models a session offers, and switching between them.

use std::sync::Arc;

use nori_acp_kit::Choice;
use nori_acp_kit::Cx;
use nori_acp_kit::MODE;
use nori_acp_kit::MODEL;
use nori_acp_kit::Options;
use nori_acp_kit::Select;
use nori_acp_kit::Sink;
use nori_acp_kit::schema::CurrentModeUpdate;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionConfigOptionCategory;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionMode;
use nori_acp_kit::schema::SessionModeId;
use nori_acp_kit::schema::SessionModeState;
use nori_acp_kit::schema::SessionUpdate;

use crate::session::PermissionMode;
use crate::session::Session;
use crate::session::SessionManager;
use crate::types::AgentConfig;
use crate::types::AgentError;
use crate::types::Result;

/// The model id that means "whatever the Claude CLI picks for this account".
/// Nothing is pinned to a dated model id, which would go stale.
const DEFAULT_MODEL_ID: &str = "default";

/// Aliases the Claude CLI resolves itself to its current model of each family.
const MODEL_ALIASES: [(&str, &str, &str); 4] = [
    (
        DEFAULT_MODEL_ID,
        "Default",
        "The Claude CLI's default for this account",
    ),
    ("sonnet", "Sonnet", "Balanced for everyday work"),
    ("opus", "Opus", "The most capable, for hard problems"),
    ("haiku", "Haiku", "The fastest, for quick answers"),
];

const MODES: [(&str, &str, &str); 5] = [
    (
        "default",
        "Default",
        "Standard behavior, prompts for dangerous operations",
    ),
    (
        "acceptEdits",
        "Accept Edits",
        "Auto-accept file edit operations",
    ),
    (
        "plan",
        "Plan Mode",
        "Planning mode, no actual tool execution",
    ),
    (
        "dontAsk",
        "Don't Ask",
        "Don't prompt for permissions, deny if not pre-approved",
    ),
    (
        "bypassPermissions",
        "Bypass Permissions",
        "Bypass all permission checks",
    ),
];

/// The permission modes, in the older `modes` shape some clients still read.
pub(super) fn mode_state(current: &str) -> SessionModeState {
    let modes = MODES
        .iter()
        .map(|(id, name, description)| SessionMode::new(*id, *name).description(*description))
        .collect();
    SessionModeState::new(current.to_owned(), modes)
}

/// The mode and model as ACP config options.
///
/// Clients that follow the stable protocol (Nori among them) read these, not the older `modes`
/// field, so without them `/model` and the mode switch have nothing to open. The model offered
/// is the configured one (a full id from settings or the environment is offered as it is), or
/// `current` when the session has switched to another.
pub(super) fn options(
    config: &AgentConfig,
    mode: PermissionMode,
    current_model: Option<&str>,
) -> Options {
    let configured = config
        .model
        .clone()
        .or_else(|| std::env::var("ANTHROPIC_MODEL").ok());
    let current = current_model
        .map(str::to_owned)
        .or(configured)
        .unwrap_or_else(|| DEFAULT_MODEL_ID.to_owned());
    let models = MODEL_ALIASES
        .iter()
        .map(|(id, name, description)| Choice::new(*id, *name).describe(*description))
        .collect();
    let modes = MODES
        .iter()
        .map(|(id, name, description)| Choice::new(*id, *name).describe(*description))
        .collect();
    Options::new(vec![
        Select::new(
            MODE,
            "Mode",
            SessionConfigOptionCategory::Mode,
            modes,
            mode.as_str(),
        )
        .describe("How Claude asks before it acts"),
        Select::new(
            MODEL,
            "Model",
            SessionConfigOptionCategory::Model,
            models,
            current,
        )
        .describe("The Claude model for the next prompt"),
    ])
}

/// The options of `session` as they now stand.
pub(super) async fn session_options(
    config: &AgentConfig,
    session: &Session,
) -> Vec<SessionConfigOption> {
    options(
        config,
        session.permission_mode().await,
        session.current_model().await.as_deref(),
    )
    .to_acp()
}

/// Switch the permission mode and tell the client.
pub(super) async fn set_mode(
    sessions: &Arc<SessionManager>,
    cx: &Cx,
    session_id: &SessionId,
    mode_id: &str,
) -> Result<()> {
    let session = sessions.get_session_or_error(session_id.0.as_ref())?;
    let mode = PermissionMode::parse(mode_id)
        .ok_or_else(|| AgentError::invalid(format!("Invalid mode: {mode_id}")))?;
    session.set_permission_mode(mode).await;
    // The SDK client needs the mode too. A failure leaves the local mode in force.
    if let Err(error) = session
        .client()
        .await
        .set_permission_mode(mode.to_sdk_mode())
        .await
    {
        tracing::warn!(session_id = %session_id.0, mode = %mode_id, %error, "SDK permission mode not set");
    }
    if let Err(error) = Sink::new(cx, session_id.clone()).update(SessionUpdate::CurrentModeUpdate(
        CurrentModeUpdate::new(SessionModeId::new(mode_id)),
    )) {
        tracing::warn!(session_id = %session_id.0, %error, "CurrentModeUpdate not sent");
    }
    Ok(())
}
