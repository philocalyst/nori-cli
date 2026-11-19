//! In-process `bash` host/MCP tool and dieted host-tool catalog helpers.

use std::path::PathBuf;
use std::sync::Arc;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::NoriClientService;
use super::tool_error;

const DEFAULT_BASH_TIMEOUT_MS: i64 = 120_000;
const MAX_BASH_TIMEOUT_MS: i64 = 600_000;

/// Git and forge wrapper tools kept callable only when explicitly opted in via
/// workflow `tools.enabled`, never in the default model-facing tool catalog.
pub(super) const OPT_IN_ONLY_TOOLS: &[&str] = &[
    "git_status",
    "git_diff",
    "git_log",
    "git_commit",
    "ci_status",
    "pull_request",
];

pub(super) const BASH_INSTRUCTIONS: &str = "Bash: run shell commands via `bash`. State (`cd`, exports, functions, aliases) persists across calls. The result starts with the status line (`exit N` or `killed: ...`); anything but `exit 0` is a failure. A script chains steps with `&&` or starts with `set -euo pipefail`, so a later `echo PASS` is never reached after a failure.";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct BashRequest {
    /// Required. POSIX/Bash command or script to run in the persistent session shell.
    command: String,
    /// Timeout in milliseconds (default 120000, max 600000).
    #[serde(default)]
    timeout_ms: Option<i64>,
}

#[tool_router(router = bash_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Run a Bash command in the session's persistent in-process shell (`cd`, `export`, functions, and aliases persist). The first line of the result is the status (`exit 0`, `exit 1`, `killed: timed out after 120s`); anything but `exit 0` is an error result. Then `stdout`, `stderr` (long output is cut in the middle), `cwd` and `duration_ms`.",
        annotations(read_only_hint = false, destructive_hint = true, open_world_hint = true),
        meta = super::super::orchestrator::always_load()
    )]
    pub(crate) async fn bash(
        &self,
        Parameters(request): Parameters<BashRequest>,
    ) -> Result<CallToolResult, McpError> {
        // A program may restrict this actor's `bash` by tool, command and path.
        if let Some(sandbox) = self
            .shared
            .orchestrator
            .seed()
            .scope
            .as_ref()
            .and_then(|restriction| restriction.sandbox.as_ref())
        {
            let cwd = self
                .shared_cwd()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
            if let Err(error) =
                nori_conduit::enforce_bash_sandbox(Some(sandbox), &cwd, &request.command)
            {
                return Ok(tool_error(error.to_string()));
            }
        }
        let session = match self.bash_session().await {
            Ok(session) => session,
            Err(error) => return Ok(tool_error(error)),
        };
        let timeout_ms = request
            .timeout_ms
            .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
            .clamp(1, MAX_BASH_TIMEOUT_MS);
        let options = nori_bash::ExecOptions {
            timeout_ms: Some(timeout_ms),
            cancel: None,
        };
        let result = {
            let mut guard = session.lock().await;
            guard.exec_with_options(&request.command, options).await
        };
        Ok(super::bash_report::report(result))
    }
}

impl NoriClientService {
    pub(super) async fn bash_session(&self) -> Result<Arc<Mutex<nori_bash::BashSession>>, String> {
        {
            let sessions = self.shared.bash_sessions.lock().await;
            if let Some(existing) = sessions.get(&self.shared.caller_key) {
                return Ok(Arc::clone(existing));
            }
        }
        let cwd = self
            .shared_cwd()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let created = nori_bash::BashSession::new(cwd)
            .await
            .map_err(|error| error.to_string())?;
        let mut sessions = self.shared.bash_sessions.lock().await;
        let entry = sessions
            .entry(self.shared.caller_key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(created)));
        Ok(Arc::clone(entry))
    }
}

/// Removes git/forge wrapper tools unless explicitly opted in via `tools.enabled`,
/// and applies `ToolSelection` filtering and description overrides.
pub(super) fn apply_tool_selection(
    tool_router: &mut ToolRouter<NoriClientService>,
    selection: Option<&nori_host_tools::ToolSelection>,
) {
    let enabled_set = selection.and_then(|s| s.enabled.as_ref());
    for opt_in_tool in OPT_IN_ONLY_TOOLS {
        let explicitly_enabled = enabled_set.is_some_and(|set| set.contains(*opt_in_tool));
        if !explicitly_enabled {
            tool_router.remove_route(opt_in_tool);
        }
    }

    if let Some(selection) = selection {
        for route in tool_router.map.values_mut() {
            if let Some(description) = selection.descriptions.get(route.attr.name.as_ref()) {
                route.attr.description = Some(description.clone().into());
            }
        }
        let names: Vec<String> = tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        for name in names {
            if selection
                .enabled
                .as_ref()
                .is_some_and(|enabled| !enabled.contains(&name))
                || selection.disabled.contains(&name)
            {
                tool_router.remove_route(&name);
            }
        }
    }
}
