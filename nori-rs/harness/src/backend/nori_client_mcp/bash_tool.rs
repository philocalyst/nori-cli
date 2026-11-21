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

pub(super) const BASH_INSTRUCTIONS: &str = "Bash: `bash` runs shell commands in a persistent shell. Anything but `exit 0` is a failure, so chain steps with `&&` or start a script with `set -euo pipefail`: a later `echo PASS` must be unreachable after a failure. Long or never-ending work (dev servers, watchers, long builds and test runs) goes in the background (`background: true`, or `job_start`): its end wakes you, so end your turn instead of polling or sleeping.";

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct BashRequest {
    /// Required. The command or script to run (Bash).
    command: String,
    /// Milliseconds to wait: default 120000, at most 600000. A command still running then is not
    /// killed: it becomes a background job.
    #[serde(default)]
    timeout_ms: Option<i64>,
    /// Where to run: `local` (this computer, the default) or one of the user's machines.
    #[serde(default)]
    machine: Option<String>,
    /// Start in this directory (it must exist there); later calls continue from where this one
    /// ends. `~/x` is under that machine's home.
    #[serde(default)]
    cwd: Option<String>,
    /// Run it as a background job and return at once with its id; its end (and any line
    /// matching `watch`) arrives later as a new message. Default false.
    #[serde(default)]
    background: Option<bool>,
    /// With `background`: a regex; output lines that match wake you, besides the job's end.
    #[serde(default)]
    watch: Option<String>,
}

#[tool_router(router = bash_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "Run a shell command in your persistent session shell, or on one of the user's machines (`machine`). `cd`, exports, functions and aliases carry over between calls on this computer; on a machine only `cd` does. Quote paths with spaces or special characters, and prefer your file read, search and edit tools to `cat`, `grep`, `find` and `sed` when you have them. The result's first line is the status (`exit 0`, `exit 1 on mothership`, `running: ... now background job ...`); anything but `exit 0` is an error result. Then `stdout`, `stderr` (long output is cut in the middle), `cwd` and `duration_ms`. `timeout_ms` never kills a command: one still running becomes a background job, and the call returns its id and the output so far (here you get a fresh shell in the same directory). `background: true` returns a job at once, for servers, watchers, long builds and test runs; `watch` (a regex) also wakes you on matching lines. Do not poll or sleep: end your turn and the job wakes you. A `cwd` that does not exist, or an unreachable machine, means nothing ran.",
        annotations(read_only_hint = false, destructive_hint = true, open_world_hint = true),
        meta = super::super::orchestrator::always_load()
    )]
    pub(crate) async fn bash(
        &self,
        Parameters(request): Parameters<BashRequest>,
    ) -> Result<CallToolResult, McpError> {
        // A program may restrict this actor's `bash` by tool, command and path; such an actor
        // runs on this computer only, where those paths mean something.
        let restricted = self
            .shared
            .orchestrator
            .seed()
            .scope
            .as_ref()
            .is_some_and(|restriction| restriction.sandbox.is_some());
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
        let timeout_ms = request
            .timeout_ms
            .unwrap_or(DEFAULT_BASH_TIMEOUT_MS)
            .clamp(1, MAX_BASH_TIMEOUT_MS);
        let roster = nori_machines::global_ready().await;
        let machine = match roster.resolve(request.machine.as_deref()) {
            Ok(nori_machines::Target::Local) => None,
            Ok(nori_machines::Target::Remote(machine)) => Some(machine.clone()),
            Err(error) => return Ok(tool_error(error.to_string())),
        };
        if restricted && let Some(machine) = &machine {
            return Ok(tool_error(format!(
                "this actor's bash is restricted to this computer; it cannot run on {}",
                machine.name
            )));
        }
        if request.background == Some(true) {
            return Ok(self.bash_background(&request).await);
        }
        if let Some(machine) = machine {
            return Ok(self
                .bash_remote(&roster, &machine, &request, timeout_ms)
                .await);
        }
        let session = match self.bash_session().await {
            Ok(session) => session,
            Err(error) => return Ok(tool_error(error)),
        };
        let cwd = {
            let mut guard = session.lock().await;
            if let Some(cwd) = request.cwd.as_deref() {
                let dir = std::path::Path::new(cwd);
                let dir = if dir.is_absolute() {
                    dir.to_path_buf()
                } else {
                    guard.cwd().join(dir)
                };
                if let Err(error) = guard.set_cwd(&dir) {
                    return Ok(tool_error(format!(
                        "cannot start in {}: {error}. The command did not run.",
                        dir.display()
                    )));
                }
            }
            // What a fresh shell needs to carry on as this one, should the command outlive
            // its timeout and keep this shell: exports, aliases and functions as they are now.
            let snapshot = guard
                .exec_with_options(
                    super::bash_job::SNAPSHOT,
                    nori_bash::ExecOptions {
                        timeout_ms: Some(2_000),
                        cancel: None,
                    },
                )
                .await
                .ok()
                .filter(|output| output.exit_code == 0)
                .map(|output| output.stdout);
            (guard.cwd().to_path_buf(), snapshot)
        };
        let (cwd, snapshot) = cwd;
        let mut running = super::bash_job::start(Arc::clone(&session), request.command.clone());
        let timeout = std::time::Duration::from_millis(timeout_ms as u64);
        match tokio::time::timeout(timeout, &mut running.done).await {
            Ok(Ok(result)) => Ok(super::bash_report::report(result)),
            Ok(Err(_)) => Ok(tool_error("the shell stopped before the command ended")),
            // Still running: it carries on as a job, and the caller gets a fresh shell here.
            Err(_) => Ok(self
                .bash_outlived(running, &request.command, &cwd, snapshot, timeout_ms)
                .await),
        }
    }
}

impl NoriClientService {
    /// `background: true`: a job, on this computer or a machine, through the session's jobs.
    async fn bash_background(&self, request: &BashRequest) -> CallToolResult {
        let local_cwd = match self.bash_session().await {
            Ok(session) => session
                .try_lock()
                .map(|guard| guard.cwd().to_path_buf())
                .ok(),
            Err(_) => None,
        }
        .or_else(|| self.shared_cwd())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")));
        let start = crate::backend::jobs::StartJob {
            command: request.command.clone(),
            label: Some(label_of(&request.command)),
            machine: request.machine.clone(),
            cwd: request.cwd.clone(),
            env: std::collections::BTreeMap::new(),
            origin: crate::backend::jobs::JobOrigin::Bash { call_id: None },
            wake: crate::backend::jobs::JobWake {
                on_exit: true,
                on_output: request.watch.clone(),
                ready: None,
            },
            local_cwd,
        };
        match self.orchestrator().jobs().start(start).await {
            Ok(summary) => super::bash_report::report_background(&summary),
            Err(error) => tool_error(error),
        }
    }

    /// A local command that outlived its timeout: follow it as a job, and give the caller a
    /// fresh shell in the directory it was in.
    async fn bash_outlived(
        &self,
        running: super::bash_job::Running,
        command: &str,
        cwd: &std::path::Path,
        snapshot: Option<String>,
        timeout_ms: i64,
    ) -> CallToolResult {
        let (stdout, stderr) = running.output_so_far();
        let job = running.into_job(label_of(command), command.to_owned());
        let summary = self
            .orchestrator()
            .jobs()
            .adopt_handle(
                job,
                crate::backend::jobs::JobOrigin::Bash { call_id: None },
                crate::backend::jobs::JobWake::default(),
            )
            .await;
        let carried = match nori_bash::BashSession::new(cwd.to_path_buf()).await {
            Ok(mut fresh) => {
                let carried = super::bash_job::restore(&mut fresh, snapshot.as_deref()).await;
                self.shared
                    .bash_sessions
                    .lock()
                    .await
                    .insert(self.shared.caller_key.clone(), Arc::new(Mutex::new(fresh)));
                carried
            }
            Err(error) => {
                tracing::warn!("could not start a fresh shell: {error}");
                super::bash_job::Carried::default()
            }
        };
        super::bash_report::report_outlived(timeout_ms, summary, &stdout, &stderr, Some(&carried))
    }
}

/// Said after a command became a job.
pub(super) const BACKGROUND_NEXT: &str = "Running in the background. Its end (and any line matching `watch`) arrives as a new message, even after your turn ends: do other work or end your turn. Do not poll or sleep. `job_read` shows its output, `job_stop` stops it.";

impl NoriClientService {
    /// Run on another machine: in the directory named, else where the caller's last command
    /// there ended, else this project's path under its home, its configured directory, or home.
    async fn bash_remote(
        &self,
        roster: &nori_machines::Roster,
        machine: &nori_machines::Machine,
        request: &BashRequest,
        timeout_ms: i64,
    ) -> CallToolResult {
        let key = (self.shared.caller_key.clone(), machine.name.clone());
        let start = match request.cwd.as_deref() {
            Some(cwd) => nori_machines::StartDir::Required(nori_machines::RemoteDir::parse(cwd)),
            None => {
                let local = self.shared_cwd().unwrap_or_else(|| {
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
                });
                let defaults = roster.default_dirs(machine, &local);
                let previous = self.shared.remote_dirs.lock().await.get(&key).cloned();
                nori_machines::StartDir::FirstOf(
                    previous
                        .map(nori_machines::RemoteDir::Path)
                        .into_iter()
                        .chain(defaults)
                        .collect(),
                )
            }
        };
        // On the job substrate from the start: the command runs detached on the machine, so a
        // timeout or a dropped connection only stops the waiting, and the command carries on
        // as a background job.
        use nori_machines::jobs::Supervisor as _;
        let supervisor = nori_machines::jobs::MachineSupervisor::of_this_process();
        let spec = nori_machines::jobs::JobSpec {
            command: request.command.clone(),
            place: nori_machines::jobs::Place::Machine(machine.name.clone()),
            start,
            env: std::collections::BTreeMap::new(),
            label: label_of(&request.command),
        };
        let job = match supervisor.start(spec).await {
            Ok(job) => job,
            Err(nori_machines::jobs::JobError::Machine(error)) => {
                return super::bash_report::report_remote(&machine.name, Err(error));
            }
            Err(error) => return tool_error(format!("did not run on {}: {error}", machine.name)),
        };
        let started = std::time::Instant::now();
        let timeout = std::time::Duration::from_millis(timeout_ms as u64);
        let state = job.wait(timeout).await;
        if !state.is_final() {
            let (stdout, stderr) = match job
                .read(
                    nori_machines::jobs::ReadFrom::Cursor(nori_machines::jobs::Cursor::default()),
                    super::bash_job::SO_FAR_BYTES,
                )
                .await
            {
                Ok(output) => super::bash_job::streams(&output),
                Err(_) => (String::new(), String::new()),
            };
            let summary = self
                .orchestrator()
                .jobs()
                .adopt_handle(
                    job,
                    crate::backend::jobs::JobOrigin::Bash { call_id: None },
                    crate::backend::jobs::JobWake::default(),
                )
                .await;
            return super::bash_report::report_outlived(
                timeout_ms, summary, &stdout, &stderr, None,
            );
        }
        let output = job
            .read(
                nori_machines::jobs::ReadFrom::Everything,
                super::bash_job::SO_FAR_BYTES,
            )
            .await;
        if let Some(cwd) = job.final_cwd() {
            self.shared.remote_dirs.lock().await.insert(key, cwd);
        }
        let cwd = job.final_cwd().unwrap_or_default();
        // An ended foreground command is nobody's job: its files go, without holding the result.
        let forget = job.clone();
        tokio::spawn(async move {
            let _ = forget.forget().await;
        });
        let (stdout, stderr) = match &output {
            Ok(output) => super::bash_job::streams(output),
            Err(error) => (
                String::new(),
                format!("[the output could not be read: {error}]"),
            ),
        };
        let exit_code = match state {
            nori_machines::jobs::JobState::Exited { code } => code,
            nori_machines::jobs::JobState::Killed { signal } => 128 + signal_number(&signal),
            nori_machines::jobs::JobState::Lost { reason }
            | nori_machines::jobs::JobState::Unreachable { reason } => {
                return tool_error(format!(
                    "the command on {} was lost: {reason}",
                    machine.name
                ));
            }
            nori_machines::jobs::JobState::Running { .. } => -1,
        };
        super::bash_report::report_remote(
            &machine.name,
            Ok(nori_machines::Run {
                exit_code,
                stdout,
                stderr,
                started_in: cwd.clone(),
                cwd,
                duration_ms: started.elapsed().as_millis() as i64,
            }),
        )
    }
}

/// What a command's job is called: its first line, short.
fn label_of(command: &str) -> String {
    command
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or(command)
        .trim()
        .chars()
        .take(48)
        .collect()
}

/// The conventional number of a signal a job was killed by.
fn signal_number(signal: &str) -> i64 {
    match signal {
        "HUP" => 1,
        "INT" => 2,
        "KILL" => 9,
        "USR1" => 10,
        "USR2" => 12,
        _ => 15,
    }
}

/// Tell the model which machines `machine` may name: an enum it cannot misspell, each one
/// described. Without machines the parameter says only `local`.
pub(super) fn describe_machines(
    tool_router: &mut ToolRouter<NoriClientService>,
    roster: &nori_machines::Roster,
) {
    for tool in ["bash", "read", "write", "edit", "multi_edit", "apply_patch"] {
        describe_machines_for(tool_router, roster, tool);
    }
}

fn describe_machines_for(
    tool_router: &mut ToolRouter<NoriClientService>,
    roster: &nori_machines::Roster,
    tool: &str,
) {
    let Some(route) = tool_router.map.get_mut(tool) else {
        return;
    };
    let mut schema = (*route.attr.input_schema).clone();
    let Some(serde_json::Value::Object(machine)) = schema
        .get_mut("properties")
        .and_then(|properties| properties.get_mut("machine"))
    else {
        return;
    };
    let mut description = if tool == "bash" {
        "Where to run: `local` (this computer, the default)".to_owned()
    } else {
        "Whose files: `local` (this computer, the default)".to_owned()
    };
    if roster.is_empty() {
        description.push_str(". No other machines are set up.");
    } else {
        description.push_str(" or one of the user's machines, which Nori reaches itself (never wrap a command in ssh):\n");
        description.push_str(&roster.describe().join("\n"));
    }
    machine.insert("type".to_owned(), serde_json::json!("string"));
    machine.remove("anyOf");
    machine.insert("enum".to_owned(), serde_json::json!(roster.names()));
    machine.insert("description".to_owned(), serde_json::json!(description));
    route.attr.input_schema = Arc::new(schema);
}

/// What the agent is told about the user's machines, when there are any.
pub(super) fn machines_guide(roster: &nori_machines::Roster) -> Option<String> {
    if roster.is_empty() {
        return None;
    }
    Some(format!(
        "Machines: `bash` runs on this computer unless you name one of the user's machines in `machine`. \
Nori connects to them itself, so never wrap a command in `ssh` or `rex`; name the machine instead.\n{}\n\
On a machine each call starts a fresh shell (bash when installed) with that machine's login PATH, in the directory the \
previous call there ended in; the first starts in this project's path under that machine's home when it exists, else in \
its home. `cd` carries over between calls; exports, functions and aliases do not (chain them with `&&`). `cwd` names a \
directory to start in, which must exist. Nori's file tools (read, write, edit, apply_patch, when you have them) take \
`machine` too; any other file tool sees only this computer, so on a machine use those or `bash` there. An unreachable \
machine is reported as such and nothing runs. \
Examples: {{\"command\": \"uname -a && pwd\", \"machine\": \"{first}\"}}, \
{{\"command\": \"git status --short\", \"machine\": \"{first}\", \"cwd\": \"~/src/app\"}}.",
        roster.describe().join("\n"),
        first = roster
            .machines()
            .next()
            .map_or_else(String::new, |machine| machine.name.to_string()),
    ))
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
