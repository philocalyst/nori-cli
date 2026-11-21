//! The job tools of the `nori-client` server: run a command in the background, and be woken by it.

use serde::Serialize;
use serde_json::json;

use super::*;
use crate::backend::jobs::JobReadRequest;
use crate::backend::jobs::JobStartRequest;
use crate::backend::jobs::JobStopRequest;

/// Said after a job is started.
const STARTED: &str = "Running. A new message wakes you when it is ready, prints a line you asked to hear, or exits, even after your turn ends: do other work or end your turn. Do not poll or sleep.";
/// Said with each read.
const READ: &str = "Read what is new, once. Do not read in a loop to wait: the job wakes you.";

/// `body` with what to do next.
fn with_next(body: &impl Serialize, next: &str) -> CallToolResult {
    let mut body = serde_json::to_value(body).unwrap_or_else(|_| json!({}));
    if let Some(map) = body.as_object_mut() {
        map.insert("next".to_owned(), json!(next));
    }
    tool_success(body)
}

#[tool_router(router = job_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Start a command in the background and return at once with its job id. It keeps running after your turn, the session and Nori end (a resumed session finds it again), on this computer or one of the user's machines (`machine`); `job` starts a job module by name instead of `command`. You are woken with a new message when it is ready (`ready`: a regex over its output), prints a line matching `wake_on_output`, or exits (`wake_on_exit`, default on; a non-zero exit, a kill or a lost job always wakes you). Its output is kept for `job_read`. Use it for a dev server, a watcher, a long build or test run, and never poll or `sleep` for it. Not for a command whose result you need now (use `bash`), nor for watching a file, URL, CI or a peer (use `monitor_start`). An error (a bad regex, an unknown module or machine) means nothing started. Examples: `{\"label\":\"dev\",\"command\":\"pnpm dev\",\"cwd\":\"web\",\"ready\":\"Local:\",\"wake_on_output\":\"(?i)error|panic\"}`; a long test run elsewhere: `{\"label\":\"tests\",\"command\":\"cargo test 2>&1\",\"machine\":\"h16003mac\"}`.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true),
        meta = super::super::orchestrator::always_load()
    )]
    pub(crate) async fn job_start(
        &self,
        Parameters(request): Parameters<JobStartRequest>,
    ) -> Result<CallToolResult, McpError> {
        let cwd = self.monitor_cwd().await;
        let modules = self.orchestrator().modules().await;
        let start = match request.into_start(&modules, &cwd) {
            Ok(start) => start,
            Err(error) => return Ok(tool_error(error)),
        };
        // A program may restrict this actor's commands; such an actor runs on this computer only.
        let sandbox = self
            .shared
            .orchestrator
            .seed()
            .scope
            .as_ref()
            .and_then(|scope| scope.sandbox.clone());
        if let Some(sandbox) = &sandbox {
            let remote = start
                .machine
                .as_deref()
                .is_some_and(|machine| !machine.trim().eq_ignore_ascii_case(nori_machines::LOCAL));
            if remote {
                return Ok(tool_error(
                    "this actor's commands are restricted to this computer; a job cannot run on a machine",
                ));
            }
            if let Err(error) =
                nori_conduit::enforce_bash_sandbox(Some(sandbox), &cwd, &start.command)
            {
                return Ok(tool_error(error.to_string()));
            }
        }
        match self.orchestrator().jobs().start(start).await {
            Ok(summary) => Ok(with_next(&summary, STARTED)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        description = "List your background jobs: id, label, command, machine, state (`running`, `exited N`, `killed by SIGNAL`, `unreachable`, `lost`), what wakes you, and how long each has run. Use it to find a job's id or label; it is not a way to wait, because a job wakes you by itself.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn job_list(&self) -> Result<CallToolResult, McpError> {
        let jobs = self.orchestrator().jobs().list();
        let next = if jobs.is_empty() {
            "You have no jobs. `job_start` starts one."
        } else {
            "Running jobs wake you by themselves; you need not call this again."
        };
        Ok(with_next(&json!({ "jobs": jobs }), next))
    }

    #[tool(
        description = "Read a background job's output now. By default (`from: new`) it returns exactly what you have not read yet, `stdout` and `stderr` apart, and moves your place on, so no line comes back twice, even across a restart. `from: tail` returns the last `max_bytes` of each stream; `from: start` the beginning. Also returns the job's state and `more: true` when output is still waiting (read again). For looking at output after a wake; it does not wait, so never read in a loop.",
        annotations(
            read_only_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn job_read(
        &self,
        Parameters(request): Parameters<JobReadRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (from, max_bytes) = request.read();
        match self
            .orchestrator()
            .jobs()
            .read(request.job.trim(), from, max_bytes)
            .await
        {
            Ok(read) => Ok(with_next(&read, READ)),
            Err(error) => Ok(tool_error(error)),
        }
    }

    #[tool(
        description = "Stop a background job by id or label: TERM to its whole process group, then KILL after `grace_seconds` (default 5). With `signal` (`INT`, `TERM`, `HUP`, `KILL`, `USR1`, `USR2`) it sends only that and leaves the rest to the job. Returns the job's state. A stop you ask for does not wake you.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            open_world_hint = true
        )
    )]
    pub(crate) async fn job_stop(
        &self,
        Parameters(request): Parameters<JobStopRequest>,
    ) -> Result<CallToolResult, McpError> {
        let (signal, grace) = request.stop();
        match self
            .orchestrator()
            .jobs()
            .stop(request.job.trim(), signal, grace)
            .await
        {
            Ok(summary) => Ok(with_next(&summary, "Stopped as asked; nothing more to do.")),
            Err(error) => Ok(tool_error(error)),
        }
    }
}
