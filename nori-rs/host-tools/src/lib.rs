//! Tools Nori gives the agents that run inside its own process.
//!
//! Nori offers every agent a few tools of its own: git status and commit, goals, and delegated
//! tasks. An agent in another process reaches them through the `nori-client` MCP server. An agent
//! built into Nori does not need a server at all: Nori registers the session's tools here, passes
//! the registration's token in the `_meta` of `session/new` (and load, resume, fork), and the
//! agent looks the token up and calls the tools directly, as functions in the same address space.
//!
//! Everything here is std and `serde_json`: the tools cross the host/agent boundary as plain data
//! and one trait object, so this crate depends on no protocol library.
//!
//! ```text
//! Nori (harness)                         built-in agent (codex-acp, claude-acp)
//!   register(tools) -> Registration
//!   session/new {_meta: session_meta()} ->  from_session_meta(meta) -> Arc<dyn HostTools>
//!                                           specs() -> the model's tool list
//!                                           call(name, args) when the model calls one
//!   tool call {_meta: {"nori":{"hostTool": name}}} <- tool_call_meta(name), for the UI
//! ```

mod selection;
pub use selection::SelectedTools;
pub use selection::ToolSelection;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use serde_json::Map;
use serde_json::Value;

/// The name the tools are grouped under where an agent needs one, such as Claude Code's
/// `mcp__nori__git_status`.
pub const SERVER_NAME: &str = "nori";

/// Guidance for the Nori-owned cross-provider task catalog.
pub const DELEGATION_INSTRUCTIONS: &str = "Nori runs a tree of agents called actors: you, the children you delegate to, their children, \
and peers. An actor has a stable address: an alias such as `reviewer` (also written `actor://reviewer`), \
or its session id (`actor://<session-id>`). A task (`task-N`) is one turn of work you gave an actor; it \
is not a new agent.
- `agent_capabilities` lists the agents and models you can delegate to. When the user names a provider \
or model, call it, then `delegate_task` with those values; another agent's own spawn tool lists only \
its own models, so do not tell the user a provider is unavailable because that tool omits it.
- `delegate_task` starts a child and returns once its session is running, or says exactly why it could \
not start. The child sees none of this conversation: give it the whole task. Give it an `alias` if \
anyone will message it again; an alias that already names a live actor reuses that actor and sends it \
the task.
- `task_send` messages any actor by `task-N`, alias, `actor://alias` or session id: the same actor \
every time, never a new child. It reads the message at its next turn boundary (now if idle; \
`steer: true` interrupts its current turn first). An ended or failed actor is revived with its \
history. A message to a child you delegated to is a task: its answer to that turn comes back to you. \
A message to anyone else is mail: they answer with their own `task_send`, and nothing returns \
automatically.
- Results and replies arrive as new messages in a new turn. Never poll and never hold your turn \
waiting: after delegating or sending, continue other work or end your turn. `queued` means the \
actor has not read the message yet; only `delivered` or `acknowledged` mean it has. Use `task_wait` \
only when you truly cannot go on without a child's result, and never to wait for a peer. Repeating \
`task_status` unchanged achieves nothing.
- Children run independently: the user interrupting your turn does not stop them, and their results still arrive as new messages. They end with this session, or when you or the user stop them (`task_cancel`).";

/// Guidance for monitors: background watchers whose events wake the agent, instead of polling.
pub const MONITOR_INSTRUCTIONS: &str = "Monitors are how you wait. Arm a background watcher with `monitor_start`, then do other work or end \
your turn. Never poll, `sleep` or hold a turn open for a build, a test run, CI, a dev server, a log, a \
file, a URL or a peer: every event the watcher produces reaches you as a new `<monitor_events>` \
message, even after your turn ended.
- `source`: `command` (a shell command; each stdout line is an event, `stderr: true` adds stderr; when \
it exits its exit code is the final event and non-zero is a failure), `file` (tail a file; `from_start` \
replays it first), `watch` (`paths`: created, modified and deleted entries, debounced), `actor` (a \
peer's state changes, results and replies; children you delegated to already wake you with \
their results), `ci` (the checks on a commit; ends with the verdict and \
the log of each failed job), `http_poll` (an event only when the value picked by `json_path` or \
`regex` changes; no credentials: use `command` with curl for those).
- Cover failure, not only success: silence is not success. `fail_on` is a list of regexes that always \
emit and mark the event FAILED, even when `filter` would drop the line: \
`[\"Traceback\", \"panicked\", \"FAILED\", \"error\\\\[\", \"Killed\", \"OOM\", \"exit [1-9]\"]`. `filter` \
keeps only matching lines; `until` ends the monitor once a line matches (that line is emitted first). \
Filter with `filter`, not by piping the command through `grep`: a pipeline reports grep's exit \
status, not the command's (if you must pipe, start with `set -o pipefail`).
- A monitor ends when its source exits, at `timeout_seconds` (default 1800; you are told it expired and \
how many events it saw, which is not success), at `until`, at `max_events` (a monitor that floods \
stops and says so: narrow `filter` or raise `batch_ms`), on `monitor_stop`, or with this session. It \
survives your turn ending. After a restart it is dormant: `monitor_rearm` starts it again.
- `wake`: `immediate` (default) wakes you when you are idle as soon as events arrive; `digest` gathers \
events for `digest_seconds` (default 30) into one wake; `boundary` never wakes an idle you, events \
come after your next turn. A failure, or a monitor ending in failure, always wakes you.
- Give each a short `label`. At most 8 run at once. A short command you need now is not a monitor: \
run it with `bash`. `monitor_list` shows what is armed and hands you events you have not read; \
`monitor_stop` ends one (or `all`); `monitor_rearm` restarts an ended or dormant monitor and extends a \
running one.
Examples: tests: {\"label\":\"tests\",\"source\":\"command\",\"command\":\"cargo test 2>&1\",\"filter\":\"test result|FAILED|panicked|error\",\"fail_on\":[\"FAILED\",\"panicked\",\"error\\\\[E\"],\"timeout_seconds\":900}. \
CI on the branch: {\"label\":\"ci\",\"source\":\"ci\"}. Dev server: {\"label\":\"dev\",\"source\":\"file\",\"path\":\"/tmp/dev.log\",\"filter\":\"ERROR|WARN\",\"fail_on\":[\"Traceback\",\"panicked\",\"Killed\",\"OOM\"],\"wake\":\"digest\"}. \
A peer: {\"label\":\"review\",\"source\":\"actor\",\"actor\":\"reviewer\"}. Source changes: {\"label\":\"src\",\"source\":\"watch\",\"paths\":[\"src\"],\"kinds\":[\"modified\"],\"filter\":\"\\\\.rs$\"}. \
A deploy: {\"label\":\"deploy\",\"source\":\"http_poll\",\"url\":\"https://example.com/status\",\"json_path\":\"$.state\",\"interval_seconds\":20,\"until\":\"live|failed\"}.";

/// The `_meta` key under which everything here lives.
const META_KEY: &str = "nori";
/// In a session request's `_meta.nori`: the registration token.
const SESSION_TOKEN_KEY: &str = "hostTools";
/// In an initialize response's `_meta.nori`: whether the agent takes host tools.
const ADVERTISE_KEY: &str = "hostTools";
/// In a tool call's `_meta.nori`: which host tool it is.
const TOOL_CALL_KEY: &str = "hostTool";

/// One tool, in the shape an MCP `tools/list` entry has.
#[derive(Debug, Clone, PartialEq)]
pub struct HostToolSpec {
    pub name: String,
    pub description: String,
    /// The JSON Schema of the arguments: always an object schema.
    pub input_schema: Value,
    /// MCP tool annotations (`readOnlyHint`, ...), when the tool has any.
    pub annotations: Option<Value>,
    /// The MCP tool `_meta`, such as `anthropic/alwaysLoad`.
    pub meta: Option<Map<String, Value>>,
}

/// What a call returned: text for the model, usually JSON, and whether it failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolOutput {
    pub text: String,
    pub is_error: bool,
    /// Complete serialized MCP result, preserving content blocks, structuredContent,
    /// result metadata and future fields. None means a legacy text-only result.
    pub mcp_result: Option<Value>,
    /// Session-scoped model context, separate from the canonical tool result.
    pub model_context: Option<String>,
}

impl HostToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            mcp_result: None,
            model_context: None,
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            mcp_result: None,
            model_context: None,
        }
    }

    /// Keep the complete MCP response while deriving a legacy text view.
    pub fn from_mcp_result(result: Value) -> Self {
        let text = result
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|block| {
                (block.get("type")?.as_str()? == "text")
                    .then(|| block.get("text")?.as_str())
                    .flatten()
            })
            .collect::<Vec<_>>()
            .join("\n");
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Self {
            text,
            is_error,
            mcp_result: Some(result),
            model_context: None,
        }
    }

    /// The complete MCP result, synthesizing a text block for older implementations.
    pub fn as_mcp_result(&self) -> Value {
        self.mcp_result.clone().unwrap_or_else(|| {
            serde_json::json!({
                "content": [{"type": "text", "text": self.text}], "isError": self.is_error,
            })
        })
    }

    /// A cancellation receipt retains the originating call ID and tool name.
    pub fn cancelled(call_id: &str, tool: &str) -> Self {
        Self::from_mcp_result(serde_json::json!({
            "content": [{"type": "text", "text": format!("Tool call {call_id} was cancelled")}],
            "structuredContent": {"status": "cancelled", "call_id": call_id, "tool": tool},
            "isError": true,
        }))
    }
}

pub type HostToolFuture = Pin<Box<dyn Future<Output = HostToolOutput> + Send>>;

/// One session's tools, as the host implements them.
pub trait HostTools: Send + Sync {
    /// What the agent should be told about the tools, as MCP server instructions are.
    fn instructions(&self) -> String;

    /// Every tool the agent may call.
    fn specs(&self) -> Vec<HostToolSpec>;

    /// Run the tool `name`. An unknown name or bad arguments come back as an error output, never
    /// a panic: the model reads the error and tries again.
    fn call(&self, name: &str, arguments: Value) -> HostToolFuture;

    /// Bind the root ACP session that owns this registration. Implementations
    /// may use this trusted identity to scope conversation-local context.
    fn bind_session_id(&self, _session_id: &str) {}

    /// Call from a known ACP session. Legacy implementations remain unscoped.
    fn call_scoped(&self, name: &str, arguments: Value, _session_id: &str) -> HostToolFuture {
        self.call(name, arguments)
    }
}

/// What every session's tools look like, before any session has them. Every registration has
/// the same tools; only the state they act on differs. An agent that prepares a session ahead
/// of time (to have it ready the moment one is asked for) lists these, and binds the session's
/// own tools when it is claimed.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub specs: Vec<HostToolSpec>,
    pub instructions: String,
}

fn catalog_slot() -> &'static Mutex<Option<Arc<Catalog>>> {
    static CATALOG: OnceLock<Mutex<Option<Arc<Catalog>>>> = OnceLock::new();
    CATALOG.get_or_init(Default::default)
}

/// Publish the tools every session will have.
pub fn publish_catalog(catalog: Catalog) {
    *catalog_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(catalog));
}

/// The tools every session will have, once the host has published them.
pub fn catalog() -> Option<Arc<Catalog>> {
    catalog_slot()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn registry() -> &'static Mutex<HashMap<String, Arc<dyn HostTools>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<dyn HostTools>>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// A session's tools, findable by token until this is dropped. An agent that already looked
/// them up keeps its own handle.
#[derive(Debug)]
pub struct Registration {
    token: String,
}

impl Registration {
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The `_meta` to send with a session request so a built-in agent finds these tools.
    pub fn session_meta(&self) -> Map<String, Value> {
        nori_meta(SESSION_TOKEN_KEY, Value::String(self.token.clone()))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.token);
    }
}

/// Make `tools` findable by built-in agents.
pub fn register(tools: Arc<dyn HostTools>) -> Registration {
    static NEXT: AtomicI64 = AtomicI64::new(1);
    // Tokens only need to be unique: the registry never leaves this process.
    let token = format!("host-tools-{}", NEXT.fetch_add(1, Ordering::Relaxed));
    registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(token.clone(), tools);
    Registration { token }
}

/// The tools a session request's `_meta` names, if it names registered ones.
pub fn from_session_meta(meta: Option<&Map<String, Value>>) -> Option<Arc<dyn HostTools>> {
    let token = meta?.get(META_KEY)?.get(SESSION_TOKEN_KEY)?.as_str()?;
    registry()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(token)
        .cloned()
}

/// The initialize-response `_meta` with which a built-in agent says it takes host tools. Only an
/// agent in Nori's own process may say so, since the tokens mean nothing anywhere else.
pub fn advertise(meta: &mut Map<String, Value>) {
    let entry = meta
        .entry(META_KEY)
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(nori) = entry {
        nori.insert(ADVERTISE_KEY.to_owned(), Value::Bool(true));
    }
}

/// Whether an initialize response's `_meta` says the agent takes host tools.
pub fn advertised(meta: Option<&Map<String, Value>>) -> bool {
    meta.and_then(|meta| meta.get(META_KEY))
        .and_then(|nori| nori.get(ADVERTISE_KEY))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The `_meta` a built-in agent puts on the ACP tool call for host tool `name`, so a client knows
/// the call without parsing its title.
pub fn tool_call_meta(name: &str) -> Map<String, Value> {
    nori_meta(TOOL_CALL_KEY, Value::String(name.to_owned()))
}

/// The host tool an ACP tool call's `_meta` names.
pub fn host_tool_of(meta: Option<&Map<String, Value>>) -> Option<&str> {
    meta?.get(META_KEY)?.get(TOOL_CALL_KEY)?.as_str()
}

fn nori_meta(key: &str, value: Value) -> Map<String, Value> {
    let mut nori = Map::new();
    nori.insert(key.to_owned(), value);
    let mut meta = Map::new();
    meta.insert(META_KEY.to_owned(), Value::Object(nori));
    meta
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    struct Echo;

    impl HostTools for Echo {
        fn instructions(&self) -> String {
            "echo things".to_owned()
        }

        fn specs(&self) -> Vec<HostToolSpec> {
            vec![HostToolSpec {
                name: "echo".to_owned(),
                description: "Echo the arguments.".to_owned(),
                input_schema: json!({"type": "object"}),
                annotations: None,
                meta: None,
            }]
        }

        fn call(&self, name: &str, arguments: Value) -> HostToolFuture {
            let output = HostToolOutput::text(format!("{name} {arguments}"));
            Box::pin(async move { output })
        }
    }

    #[test]
    fn a_session_meta_finds_its_tools_until_the_registration_is_dropped() {
        let registration = register(Arc::new(Echo));
        let meta = registration.session_meta();
        assert_eq!(
            Value::Object(meta.clone()),
            json!({"nori": {"hostTools": registration.token()}})
        );
        let tools = from_session_meta(Some(&meta)).expect("registered");
        assert_eq!(tools.instructions(), "echo things");

        drop(registration);
        assert!(from_session_meta(Some(&meta)).is_none());
        // The agent's own handle outlives the registration.
        assert_eq!(tools.specs()[0].name, "echo");
    }

    #[test]
    fn unrelated_meta_names_no_tools() {
        assert!(from_session_meta(None).is_none());
        let meta = json!({"nori": {"hostTools": 7}, "other": true});
        assert!(from_session_meta(meta.as_object()).is_none());
    }

    #[test]
    fn the_advertisement_merges_into_existing_meta() {
        let mut meta = json!({"goal": {"version": 1}, "nori": {"other": 1}})
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(!advertised(Some(&meta)));
        advertise(&mut meta);
        assert!(advertised(Some(&meta)));
        assert_eq!(
            Value::Object(meta),
            json!({"goal": {"version": 1}, "nori": {"other": 1, "hostTools": true}})
        );
    }

    #[test]
    fn a_published_catalog_is_shared() {
        publish_catalog(Catalog {
            specs: Echo.specs(),
            instructions: "echo things".to_owned(),
        });
        assert_eq!(
            catalog().map(|catalog| catalog.specs[0].name.clone()),
            Some("echo".to_owned())
        );
    }

    #[test]
    fn a_tool_call_names_its_host_tool() {
        let meta = tool_call_meta("git_commit");
        assert_eq!(host_tool_of(Some(&meta)), Some("git_commit"));
        assert_eq!(host_tool_of(None), None);
    }
}
