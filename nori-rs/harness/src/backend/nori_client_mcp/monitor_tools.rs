//! The monitor tools of the `nori-client` server: arm a background watcher, and end your turn.

use std::path::PathBuf;

use serde_json::json;

use super::*;
use crate::backend::monitor;
use crate::backend::monitor::MonitorRearmRequest;
use crate::backend::monitor::MonitorStartRequest;
use crate::backend::monitor::MonitorTargetRequest;
use crate::backend::monitor::Overrides;
use crate::backend::monitor::source::SourceEnv;

/// Said after a monitor is armed.
const ARMED: &str = "Armed. Its events arrive as new messages, even after your turn ends: do other work or end your turn. Do not poll or sleep.";

#[tool_router(router = monitor_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Arm a background monitor and return at once; its events arrive later as new messages, so you can end your turn. Sources: `command` (shell command; each stdout line is an event; its exit code is the last event, non-zero is a failure), `file` (tail), `watch` (created/modified/deleted under paths), `actor` (a peer's state, results, replies), `ci` (checks on a commit, ends with the verdict), `http_poll` (event when the value you pick changes). Cover failure: put failure signatures (`Traceback`, `panicked`, `FAILED`, `error\\[`, `Killed`, `OOM`) in `fail_on`; silence is not success. `filter` keeps matching lines, `until` ends the monitor at a matching line, `timeout_seconds` bounds it (expiry is reported, not success), `wake` chooses `immediate`, `digest` or `boundary`.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true),
        meta = super::super::orchestrator::always_load()
    )]
    pub(crate) async fn monitor_start(
        &self,
        Parameters(request): Parameters<MonitorStartRequest>,
    ) -> Result<CallToolResult, McpError> {
        let cwd = self.monitor_cwd().await;
        let spec = match request.into_spec(&cwd) {
            Ok(spec) => spec,
            Err(error) => return Ok(tool_error(error)),
        };
        let env = match self.monitor_env(&spec.source, &cwd).await {
            Ok(env) => env,
            Err(error) => return Ok(tool_error(error)),
        };
        match self.orchestrator().monitors().start(spec, env, false).await {
            Ok(armed) => Ok(tool_success(json!({
                "monitor": armed.monitor,
                "label": armed.label,
                "source": armed.source,
                "state": "running",
                "timeout_seconds": armed.timeout_seconds,
                "max_events": armed.max_events,
                "wake": armed.wake,
                "next": ARMED,
            }))),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        description = "List your monitors: each one's id, label, source, state (`running`, `dormant` after a restart, or `ended: ...` with how), events seen, failures, and how long until it expires. Also returns, once, the events you have not been given yet. Not a way to wait: events arrive as messages by themselves.",
        annotations(
            read_only_hint = false,
            idempotent_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn monitor_list(&self) -> Result<CallToolResult, McpError> {
        let monitors = self.orchestrator().monitors();
        let events = monitors.take_unread().await;
        let list = monitors.list();
        Ok(tool_success(json!({
            "monitors": list,
            "unread_events": monitor::events_json(&events),
            "next": if list.is_empty() {
                "You have no monitors. `monitor_start` arms one."
            } else {
                "Running monitors report by themselves; you need not call this again."
            },
        })))
    }

    #[tool(
        description = "Stop a monitor by id or label (`all` stops every running one). Events it already produced are returned here, once. A monitor that ended on its own cannot be stopped; `monitor_rearm` starts it again.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        )
    )]
    pub(crate) async fn monitor_stop(
        &self,
        Parameters(request): Parameters<MonitorTargetRequest>,
    ) -> Result<CallToolResult, McpError> {
        let monitors = self.orchestrator().monitors();
        match monitors
            .stop(request.monitor.trim(), monitor::Ending::Stopped)
            .await
        {
            Ok(stopped) => {
                let events = monitors.take_unread().await;
                Ok(tool_success(json!({
                    "stopped": stopped,
                    "unread_events": monitor::events_json(&events),
                })))
            }
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        description = "Start an ended, expired or dormant monitor again with a fresh timeout and event budget (same source and rules), or extend a running monitor's timeout. Optionally change `timeout_seconds`, `max_events`, `wake`.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn monitor_rearm(
        &self,
        Parameters(request): Parameters<MonitorRearmRequest>,
    ) -> Result<CallToolResult, McpError> {
        let monitors = self.orchestrator().monitors();
        let (id, spec, running) = match monitors.find(request.monitor.trim()) {
            Ok(found) => found,
            Err(error) => return Ok(tool_error(error)),
        };
        let cwd = self.monitor_cwd().await;
        let env = if running {
            None
        } else {
            match self.monitor_env(&spec.source, &cwd).await {
                Ok(env) => Some(env),
                Err(error) => return Ok(tool_error(error)),
            }
        };
        let overrides = Overrides {
            timeout_seconds: request.timeout_seconds,
            max_events: request.max_events,
            wake: request.wake,
        };
        match monitors.rearm(&id, overrides, env).await {
            Ok(armed) => Ok(tool_success(json!({
                "monitor": armed.monitor,
                "label": armed.label,
                "state": "running",
                "timeout_seconds": armed.timeout_seconds,
                "max_events": armed.max_events,
                "wake": armed.wake,
                "next": ARMED,
            }))),
            Err(error) => Ok(tool_error(error)),
        }
    }
}

impl NoriClientService {
    /// Where a monitor's relative paths and commands start: the directory of the caller's own
    /// shell when it has one, else the session's.
    async fn monitor_cwd(&self) -> PathBuf {
        let session = self
            .shared
            .bash_sessions
            .lock()
            .await
            .get(&self.shared.caller_key)
            .cloned();
        if let Some(session) = session
            && let Ok(session) = session.try_lock()
        {
            return session.cwd().to_path_buf();
        }
        self.shared_cwd()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")))
    }

    /// The source, checked against the actor's sandbox and resolved.
    async fn monitor_env(
        &self,
        source: &monitor::spec::Source,
        cwd: &std::path::Path,
    ) -> Result<SourceEnv, String> {
        let sandbox = self
            .shared
            .orchestrator
            .seed()
            .scope
            .as_ref()
            .and_then(|scope| scope.sandbox.clone());
        SourceEnv::prepare(source, self.orchestrator(), sandbox.as_ref(), cwd).await
    }
}
