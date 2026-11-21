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
use crate::types::AgentError;
use crate::types::Result;

/// The id of the Fast mode option.
pub(super) const FAST: &str = "fast";

/// The model id that means "whatever the Claude CLI picks for this account".
/// Nothing is pinned to a dated model id, which would go stale.
const DEFAULT_MODEL_ID: &str = "default";

/// Aliases the Claude CLI resolves itself to its current model of each family: what is offered
/// before the session's CLI has said which models the account can use.
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

/// Nori never asks permission: Claude works with every tool, or plans first and asks only
/// whether the plan is ready.
const MODES: [(&str, &str, &str); 2] = [
    (
        "bypassPermissions",
        "Build",
        "Works with every tool; nothing asks first",
    ),
    (
        "plan",
        "Plan",
        "Reads and plans; changes nothing until you approve the plan",
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

/// The mode, model and Fast mode as ACP config options.
///
/// Clients that follow the stable protocol (Nori among them) read these, not the older `modes`
/// field, so without them `/model` and the mode switch have nothing to open. The models are the
/// ones the session's Claude CLI reported for this account (`reported`, its initialize `models`),
/// so a model that ships is offered the day the CLI knows it; before the CLI has started they are
/// the family aliases. The current model is always among them.
pub(super) fn options(
    mode: PermissionMode,
    current_model: Option<&str>,
    fast: bool,
    reported: &[serde_json::Value],
) -> Options {
    let current = current_model
        .map(str::to_owned)
        .unwrap_or_else(|| DEFAULT_MODEL_ID.to_owned());
    let mut models: Vec<Choice> = reported
        .iter()
        .filter_map(|model| {
            let value = model.get("value")?.as_str()?;
            let name = model
                .get("displayName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(value);
            let choice = Choice::new(value, name);
            Some(
                match model.get("description").and_then(serde_json::Value::as_str) {
                    Some(description) => choice.describe(description),
                    None => choice,
                },
            )
        })
        .collect();
    if models.is_empty() {
        models = MODEL_ALIASES
            .iter()
            .map(|(id, name, description)| Choice::new(*id, *name).describe(*description))
            .collect();
    }
    if !models.iter().any(|choice| choice.value == current) {
        models.push(Choice::new(current.clone(), current.clone()));
    }
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
        .describe("Build, or plan first"),
        Select::new(
            MODEL,
            "Model",
            SessionConfigOptionCategory::Model,
            models,
            current,
        )
        .describe("The Claude model for the next prompt"),
        Select::new(
            FAST,
            "Fast mode",
            SessionConfigOptionCategory::Other("model_config".to_owned()),
            vec![Choice::new("off", "Standard"), Choice::new("on", "Fast")],
            if fast { "on" } else { "off" },
        )
        .describe("Faster responses on supported models"),
    ])
}

/// The options of `session` as they now stand.
pub(super) async fn session_options(session: &Session) -> Vec<SessionConfigOption> {
    options(
        session.permission_mode().await,
        session.current_model().await.as_deref(),
        session.fast_mode(),
        &session.reported_models(),
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
    // The CLI needs the mode too; it is told in the background, so the switch never waits on a
    // busy or silent CLI. A failure leaves the local mode, which the permission hooks enforce, in
    // force.
    {
        let session = Arc::clone(&session);
        let session_id = session_id.clone();
        let mode_id = mode_id.to_owned();
        tokio::spawn(async move {
            if let Err(error) = session
                .client()
                .await
                .set_permission_mode(mode.to_sdk_mode())
                .await
            {
                tracing::warn!(session_id = %session_id.0, mode = %mode_id, %error, "SDK permission mode not set");
            }
        });
    }
    if let Err(error) = Sink::new(cx, session_id.clone()).update(SessionUpdate::CurrentModeUpdate(
        CurrentModeUpdate::new(SessionModeId::new(mode_id)),
    )) {
        tracing::warn!(session_id = %session_id.0, %error, "CurrentModeUpdate not sent");
    }
    Ok(())
}
