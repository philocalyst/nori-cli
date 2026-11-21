//! The task tools of the `nori-client` MCP server.

use std::sync::Arc;
use std::time::Duration;

use rmcp::ErrorData as McpError;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::schemars;
use rmcp::tool;
use rmcp::tool_router;
use serde::Deserialize;
use serde::Serialize;

use super::delegation::Delegation;
use super::delegation::Isolation;
use super::delegation::Mode;
use super::delegation::Role;
use crate::backend::nori_client_mcp::NoriClientService;

/// `agent_capabilities` re-reads agents whose entry is older than this...
const CAPABILITIES_FRESH_FOR: Duration = Duration::from_secs(120);
/// ...waiting at most this long for an agent Nori has never read...
const CAPABILITIES_WAIT: Duration = Duration::from_secs(20);
/// ...and this long when every agent was read before, answering with that if the agents are
/// slow; the refresh goes on in the background.
const CAPABILITIES_STALE_WAIT: Duration = Duration::from_secs(3);

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct DelegateTaskRequest {
    /// Required. The whole task: every fact, file path and constraint the child needs (it sees
    /// none of your conversation), and what its final message must contain. No secrets.
    task: String,
    /// The agent to run the child on: an `agent` value from `agent_capabilities`. Default: yours.
    #[serde(default)]
    agent: Option<String>,
    /// The child's model: a value `agent_capabilities` lists for that agent. Default: the agent's own.
    #[serde(default)]
    model: Option<String>,
    /// A short title shown to the user, such as "Review the parser changes".
    #[serde(default)]
    title: Option<String>,
    /// The kind of work, which frames the child's instructions: `implementation`, `research`,
    /// `review`, `design`, `test` or `general`. It only frames the child's instructions and
    /// restricts nothing: tell a research or review child in `task` not to change files if it
    /// must not.
    #[serde(default)]
    role: Option<Role>,
    /// `shared` (default): the child works in your working directory. `worktree`: a new git
    /// worktree at your current commit, for changes that must not collide with yours (your
    /// uncommitted edits are not in it; it starts on a detached HEAD, so ask it to commit or
    /// branch and report where).
    #[serde(default)]
    isolation: Option<Isolation>,
    /// `background` (default): return at once; the result is delivered to you as a message when
    /// the task finishes. `wait`: block until it finishes or `timeout_seconds` pass.
    #[serde(default)]
    mode: Option<Mode>,
    /// With `mode: wait`, seconds to block: default 45, at most 50. A timeout does not cancel the
    /// task; its result is delivered to you when it finishes.
    #[serde(default)]
    timeout_seconds: Option<i64>,
    /// An idempotency key: delegating again with the same key returns the first task instead of
    /// starting another.
    #[serde(default)]
    client_request_id: Option<String>,
    /// Other settings for the child's agent, by setting and value, from the `settings` that
    /// `agent_capabilities` lists: for example `{"thought_level": "high"}`.
    #[serde(default)]
    options: Option<std::collections::BTreeMap<String, String>>,
    /// A one-word name for the child, such as `reviewer`, so you and other actors can message it
    /// again (`task_send`, `actor://reviewer`). An alias that already names a live actor sends it
    /// this task instead of starting a second child; an ended one is revived.
    #[serde(default)]
    alias: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskTargetRequest {
    /// Required. A `task-N` you started, or an actor: its alias, `actor://alias` or session id.
    #[serde(
        alias = "task_id",
        alias = "actor",
        alias = "id",
        alias = "to",
        alias = "recipient"
    )]
    target: String,
}

/// A list of addresses, or the one address a model wrote instead of a list.
#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(untagged)]
pub(crate) enum OneOrMany {
    Many(Vec<String>),
    One(String),
}

impl OneOrMany {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Many(many) => many,
            Self::One(one) => vec![one],
        }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskWaitRequest {
    /// Required. The tasks (`task-N`) or actors (alias) to wait for; one is enough.
    #[serde(
        alias = "task_ids",
        alias = "actors",
        alias = "target",
        alias = "task_id"
    )]
    targets: OneOrMany,
    /// Seconds to block: default 45, at most 50. A timeout cancels nothing.
    #[serde(default)]
    timeout_seconds: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskSendRequest {
    /// Required. Who gets the message: an alias, `actor://alias`, `actor://<session-id>`, or a
    /// `task-N` (the actor that ran it).
    #[serde(
        alias = "to",
        alias = "task_id",
        alias = "actor",
        alias = "recipient",
        alias = "id"
    )]
    target: String,
    /// Required. The message. The actor keeps its whole conversation, so this can refer to it.
    message: String,
    /// Interrupt the actor's current turn so it reads this message next.
    #[serde(default)]
    steer: Option<bool>,
    /// Also tell your parent when this exchange settles (neither side has mail or a turn left),
    /// so it need not poll. Replies in the exchange keep the setting.
    #[serde(default)]
    cc_parent: Option<bool>,
    /// `background` (default) or `wait`, as for `delegate_task`.
    #[serde(default)]
    mode: Option<Mode>,
    /// With `mode: wait`, seconds to block: default 45, at most 50.
    #[serde(default)]
    timeout_seconds: Option<i64>,
}

#[tool_router(router = task_tool_router, vis = "pub(crate)")]
impl NoriClientService {
    #[tool(
        description = "List the agents you can delegate to with the models and other settings (such as reasoning effort) each offers, plus your delegation depth and the number of tasks running. Call it before `delegate_task` whenever the user names an agent or model, and pass its `agent`, `model` and `settings` values on; no other tool lists the other providers' models. The first call can take a few seconds while agents are read.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn agent_capabilities(&self) -> Result<CallToolResult, McpError> {
        let inner = self.orchestrator().inner();
        let never_read = crate::registry::list_available_agents()
            .into_iter()
            .filter(|agent| agent.is_installed)
            .any(|agent| {
                super::catalog::remembered(&inner.seed.config.nori_home, &agent.agent_name)
                    .refreshed_at
                    == 0
            });
        // Re-read agents not read in the last two minutes, for as long as an answer can wait.
        let _ = tokio::time::timeout(
            if never_read {
                CAPABILITIES_WAIT
            } else {
                CAPABILITIES_STALE_WAIT
            },
            super::probe::refresh(
                Arc::clone(&inner.seed.config),
                inner.seed.cli_version.clone(),
                CAPABILITIES_FRESH_FOR,
            ),
        )
        .await;
        Ok(json(&inner.capabilities()))
    }

    #[tool(
        description = "Start a child actor on a task; returns once its session is running, or an error with the cause (nothing started). The child starts with fresh context: it sees only `task` (plus Nori's standard instructions), never this conversation, and it cannot ask questions, so put everything it needs in `task`. Its result arrives later as a new message: do other work or end your turn, and use `mode: wait` only if you cannot go on without it. Give an `alias` to message it again with `task_send`: an alias that names a live actor sends it the task instead of starting a second child. `isolation: shared` edits your files directly: give parallel children disjoint files, or use `isolation: worktree`. Delegate work that runs beside yours (research, a review, an independent change); do a quick step, or one your next step is blocked on, yourself.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true),
        meta = always_load()
    )]
    pub(crate) async fn delegate_task(
        &self,
        Parameters(request): Parameters<DelegateTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let delegation = Delegation {
            agent: request.agent,
            model: request.model,
            title: request.title,
            role: request.role,
            isolation: request.isolation.unwrap_or_default(),
            mode: request.mode.unwrap_or_default(),
            timeout: seconds(request.timeout_seconds),
            client_request_id: request.client_request_id,
            options: request.options.unwrap_or_default().into_iter().collect(),
            alias: request.alias,
            ..Delegation::new(request.task)
        };
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move { inner.delegate(delegation).await }).await,
        ))
    }

    #[tool(
        description = "Read a task (`task-N`), or an actor's latest task of yours: `status`, where its message got (`delivery`: `queued` = not read yet, `delivered`, `acknowledged`), the child's current `activity`, files changed, and its final message once finished. Reading a finished task delivers its result, so it is not also sent as a message. Messages waiting for you arrive in `inbox`. Never poll: results and replies arrive as new messages.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    pub(crate) async fn task_status(
        &self,
        Parameters(TaskTargetRequest { target }): Parameters<TaskTargetRequest>,
    ) -> Result<CallToolResult, McpError> {
        Ok(outcome(self.orchestrator().inner().status(&target).await))
    }

    #[tool(
        description = "Block until the listed tasks or actors finish, one awaits the user's approval, a message arrives for you, or the timeout passes. Use it only when you cannot continue without a child's result. It returns at once for a message to a peer (nothing comes back to it): end your turn instead, replies arrive as new messages.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false),
        meta = always_load()
    )]
    pub(crate) async fn task_wait(
        &self,
        Parameters(request): Parameters<TaskWaitRequest>,
    ) -> Result<CallToolResult, McpError> {
        let timeout = seconds(request.timeout_seconds).unwrap_or(super::DEFAULT_WAIT);
        Ok(outcome(
            self.orchestrator()
                .inner()
                .wait(&request.targets.into_vec(), timeout)
                .await,
        ))
    }

    #[tool(
        description = "Send a message to any actor: `target` is a `task-N`, an alias, `actor://alias` or a session id. The same actor always gets it, never a new child. An idle actor takes it now, a working one at its next turn boundary (`steer: true` interrupts its turn first), an ended or failed one is revived with its history. To a child you delegated to it is a task and its answer returns to you; to anyone else it is mail and they answer with their own `task_send`. Returns the task with `delivery` (`queued` = not read yet). Do not wait for a reply: end your turn. Set `cc_parent` to have your parent told when an exchange between you and a peer settles.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    pub(crate) async fn task_send(
        &self,
        Parameters(request): Parameters<TaskSendRequest>,
    ) -> Result<CallToolResult, McpError> {
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move {
                inner
                    .send_to_address(
                        &request.target,
                        request.message,
                        request.mode.unwrap_or_default(),
                        seconds(request.timeout_seconds),
                        request.steer.unwrap_or(false),
                        request.cc_parent.unwrap_or(false),
                    )
                    .await
            })
            .await,
        ))
    }

    #[tool(
        description = "Stop a task, or whatever you asked of an actor. The actor stays and can be messaged again. A task that already finished keeps its result; a cancelled task's result is not delivered.",
        annotations(
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    pub(crate) async fn task_cancel(
        &self,
        Parameters(TaskTargetRequest { target }): Parameters<TaskTargetRequest>,
    ) -> Result<CallToolResult, McpError> {
        let inner = Arc::clone(self.orchestrator().inner());
        Ok(outcome(
            detached(async move { inner.cancel(&target).await }).await,
        ))
    }

    #[tool(
        description = "List the actors you can address (alias, agent, state) and the tasks you gave them with their delivery state. Messages waiting for you arrive in `inbox`.",
        annotations(read_only_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    pub(crate) async fn task_list(&self) -> Result<CallToolResult, McpError> {
        Ok(outcome(self.orchestrator().inner().list().await))
    }
}

/// Asks Claude Code to keep a tool loaded rather than behind its tool search, for the tools an
/// agent should always know it has. The rest are found by search when needed.
pub(crate) fn always_load() -> rmcp::model::Meta {
    let mut meta = rmcp::model::Meta::new();
    meta.0.insert(
        "anthropic/alwaysLoad".to_owned(),
        serde_json::Value::Bool(true),
    );
    meta
}

/// Run a call that changes tasks on its own task, so a client that gives up on the call cannot
/// stop it halfway: the task it starts or stops is then recorded whole.
async fn detached<T: Send + 'static>(
    call: impl std::future::Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String> {
    tokio::spawn(call)
        .await
        .unwrap_or_else(|error| Err(format!("the call failed: {error}")))
}

fn seconds(value: Option<i64>) -> Option<Duration> {
    value.map(|seconds| Duration::from_secs(seconds.clamp(0, 3600) as u64))
}

fn json(value: &impl Serialize) -> CallToolResult {
    match serde_json::to_string(value) {
        Ok(text) => CallToolResult::success(vec![Content::text(text)]),
        Err(error) => CallToolResult::error(vec![Content::text(error.to_string())]),
    }
}

/// A tool result: the value as JSON, or an error the model can read and act on.
fn outcome(result: Result<impl Serialize, String>) -> CallToolResult {
    match result {
        Ok(value) => json(&value),
        Err(error) => CallToolResult::error(vec![Content::text(error)]),
    }
}
