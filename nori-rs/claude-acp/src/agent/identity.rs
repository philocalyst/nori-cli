//! Who this agent's Claude is signed in as, told to the client the way the Claude adapter does
//! (`_auth/status_update`).
//!
//! The Claude CLI answers `claude auth status --json` for the account its config directory holds:
//! whether it is signed in, as which email, organization and plan. That answer, and nothing from
//! the credential files, is what the client is told.

use std::time::Duration;

use nori_acp_kit::AuthStatus;
use nori_acp_kit::Cx;
use nori_acp_kit::announce_auth_status;
use serde_json::Value;

use crate::types::AgentConfig;

/// A hung CLI must not hold the announcement forever.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Ask the CLI who it is signed in as, and tell the client. Returns at once; says nothing when the
/// CLI cannot be run or does not answer, because a guess would be worse than silence.
pub(super) fn announce(cx: &Cx, config: &AgentConfig) {
    let cx = cx.clone();
    let program = config
        .cli_path
        .clone()
        .or_else(|| std::env::var_os("CLAUDE_CODE_EXECUTABLE").map(Into::into))
        .unwrap_or_else(|| "claude".into());
    let environment = config.environment.clone();
    tokio::spawn(async move {
        let mut command = tokio::process::Command::new(&program);
        command
            .args(["auth", "status", "--json"])
            .envs(environment)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let Ok(Ok(output)) = tokio::time::timeout(PROBE_TIMEOUT, command.output()).await else {
            return;
        };
        let Some(status) = std::str::from_utf8(&output.stdout)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(text).ok())
            .and_then(|json| AuthStatus::from_claude_cli(&json))
        else {
            return;
        };
        if let Err(error) = announce_auth_status(&cx, &status) {
            tracing::debug!(%error, "account identity not sent");
        }
    });
}
