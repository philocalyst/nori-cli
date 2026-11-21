//! The backend-owned `nori-client` MCP server.
//!
//! This is Nori's harness-side channel to the external ACP agent: the single
//! loopback MCP server through which Nori exposes harness-specific tooling that
//! the ACP protocol itself does not (yet) provide. It carries the goal tools
//! (`get_goal`/`create_goal`/`update_goal`) and the delegated-task tools (see
//! [`super::orchestrator`]); it is named `nori-client` because it is the general
//! point of contact, not a single-purpose surface.
//!
//! An agent that speaks HTTP MCP reaches it through rmcp's spec-compliant
//! streamable-HTTP server, served on a loopback port and protected by a
//! per-session bearer token before requests reach rmcp. Any other agent gets a
//! stdio server: a command that relays its stdio to a unix socket in a private
//! directory (`nori stdio-to-uds <socket>`, see [`set_stdio_bridge`]), where the
//! same service is served. The tools are typed `#[tool]` handlers; there is no
//! hand-rolled HTTP or JSON-RPC framing here.
//!
//! An agent built into Nori's own process gets the same tools with no server at all: they are
//! handed to it as host tools (see [`super::host_tools`]), and this server is not started.

mod bash_report;
mod bash_tool;
mod caller;
mod conversation_tools;
mod goal_tools;
mod job_tools;
mod lsp_tools;
mod monitor_tools;
mod server;
mod session_capabilities;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware;
use axum::middleware::Next;
use axum::response::Response;
use rmcp::ErrorData as McpError;
use rmcp::RoleServer;
use rmcp::ServerHandler;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::model::Content;
use rmcp::model::GetPromptRequestParam;
use rmcp::model::GetPromptResult;
use rmcp::model::Implementation;
use rmcp::model::InitializeRequestParam;
use rmcp::model::InitializeResult;
use rmcp::model::ListPromptsResult;
use rmcp::model::ListResourcesResult;
use rmcp::model::PaginatedRequestParam;
use rmcp::model::ProtocolVersion;
use rmcp::model::ReadResourceRequestParam;
use rmcp::model::ReadResourceResult;
use rmcp::model::ServerCapabilities;
use rmcp::model::ServerInfo;
use rmcp::schemars;
use rmcp::service::RequestContext;
use rmcp::tool;
use rmcp::tool_router;
use rmcp::transport::streamable_http_server::StreamableHttpService;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::StreamableHttpServerConfig;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use super::thread_goal::GoalStatus;
use super::*;

/// The Arcs the `nori-client` tools operate on, shared across every request the
/// streamable-HTTP transport spawns. Cloned (cheaply) into each per-session
/// [`NoriClientService`] by the service factory.
#[derive(Clone)]
pub(crate) struct NoriClientShared {
    thread_goal_state: Arc<Mutex<thread_goal::ThreadGoalState>>,
    backend_event_tx: mpsc::Sender<BackendEvent>,
    connected: Arc<AtomicBool>,
    agent_capabilities: crate::normalized::AgentCapabilitiesView,
    nori_client_advertised: bool,
    goal_ext_advertised: bool,
    orchestrator: super::orchestrator::Orchestrator,
    /// Where the git and forge tools work.
    workspace: Option<Workspace>,
    pub(super) tool_selection: Option<nori_host_tools::ToolSelection>,
    /// Persistent per-caller in-process Bash sessions (keyed by caller session ID, or `""` for root).
    bash_sessions: Arc<Mutex<HashMap<String, Arc<Mutex<nori_bash::BashSession>>>>>,
    caller_key: String,
}

/// Where a session's tools work: its directory, and the workflow whose rules they apply.
#[derive(Clone)]
pub(crate) struct Workspace {
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) workflow: Arc<Mutex<super::workflow::WorkflowState>>,
}

impl NoriClientShared {
    pub(crate) fn new(
        thread_goal_state: Arc<Mutex<thread_goal::ThreadGoalState>>,
        backend_event_tx: mpsc::Sender<BackendEvent>,
        connected: Arc<AtomicBool>,
        agent_capabilities: crate::normalized::AgentCapabilitiesView,
        nori_client_advertised: bool,
        goal_ext_advertised: bool,
        orchestrator: super::orchestrator::Orchestrator,
    ) -> Self {
        Self {
            thread_goal_state,
            backend_event_tx,
            connected,
            agent_capabilities,
            nori_client_advertised,
            goal_ext_advertised,
            orchestrator,
            workspace: None,
            tool_selection: None,
            bash_sessions: Arc::new(Mutex::new(HashMap::new())),
            caller_key: String::new(),
        }
    }

    /// Serve the git and forge tools for `workspace`.
    pub(crate) fn with_workspace(mut self, workspace: Workspace) -> Self {
        self.workspace = Some(workspace);
        self
    }
}

/// One rmcp server handler instance. The transport constructs a fresh service
/// per session via the factory; all instances share the same [`NoriClientShared`]
/// Arcs, so goal state and the connected flag are global to the backend.
#[derive(Clone)]
pub(crate) struct NoriClientService {
    shared: NoriClientShared,
    tool_router: ToolRouter<Self>,
}

impl NoriClientService {
    pub(super) async fn take_goal_context(&self) -> Option<String> {
        let mut state = self.shared.thread_goal_state.lock().await;
        if state.ext_driven() {
            return None;
        }
        state.take_goal_context(thread_goal::now_seconds())
    }

    /// The working directory the git tools run in.
    pub(super) fn shared_cwd(&self) -> Option<std::path::PathBuf> {
        self.shared
            .workspace
            .as_ref()
            .map(|workspace| workspace.cwd.clone())
    }

    /// Where the forge tools work.
    pub(super) fn workspace(&self) -> Option<&Workspace> {
        self.shared.workspace.as_ref()
    }

    pub(super) fn new(shared: NoriClientShared) -> Self {
        let mut tool_router = Self::goal_tool_router()
            + Self::bash_tool_router()
            + Self::task_tool_router()
            + Self::git_tool_router()
            + Self::forge_tool_router()
            + Self::monitor_tool_router()
            + Self::conversation_tool_router();
        bash_tool::apply_tool_selection(&mut tool_router, shared.tool_selection.as_ref());
        Self {
            shared,
            tool_router,
        }
    }

    pub(super) fn session_tools(&self) -> Vec<rmcp::model::Tool> {
        self.tool_router.list_all()
    }

    pub(super) fn has_tool(&self, name: &str) -> bool {
        self.tool_router.has_route(name)
    }

    pub(super) fn instructions(&self) -> String {
        let mut out = tool_instructions();
        if ["git_status", "git_diff", "git_log", "git_commit"]
            .iter()
            .any(|tool| self.has_tool(tool))
        {
            out.push_str("\n\n");
            out.push_str(super::git_tools::INSTRUCTIONS);
        }
        if ["ci_status", "pull_request"]
            .iter()
            .any(|tool| self.has_tool(tool))
        {
            out.push_str("\n\n");
            out.push_str(super::forge_tools::INSTRUCTIONS);
        }
        out
    }

    /// Every default model-facing tool, as `tools/list` describes it.
    pub(super) fn all_tools() -> Vec<rmcp::model::Tool> {
        let tool_router = Self::goal_tool_router()
            + Self::bash_tool_router()
            + Self::task_tool_router()
            + Self::monitor_tool_router()
            + Self::conversation_tool_router();
        tool_router.list_all()
    }
}

impl NoriClientService {
    /// The delegated tasks of the session this server belongs to.
    pub(crate) fn orchestrator(&self) -> &super::orchestrator::Orchestrator {
        &self.shared.orchestrator
    }
}

impl ServerHandler for NoriClientService {
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<rmcp::model::ListToolsResult, McpError> {
        Ok(rmcp::model::ListToolsResult {
            tools: self.tool_router.list_all(),
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let caller = caller::session_from_meta(&context.meta)?;
        let service = self.for_caller(caller, request.name.as_ref()).await?;
        let call = rmcp::handler::server::tool::ToolCallContext::new(&service, request, context);
        service.tool_router.call(call).await
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .enable_prompts()
                .build(),
            server_info: Implementation {
                name: "nori-client".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            },
            instructions: Some(format!(
                "Use nori-client resources and prompts for Nori CLI harness context; use tools only for Nori-owned live state.\n\n{}",
                super::host_tools::from_shared(self.shared.clone()).instructions()
            )),
        }
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        Ok(nori_client_context::list_resources())
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        nori_client_context::read_resource(request.uri)
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParam>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(nori_client_context::list_prompts())
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParam,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        nori_client_context::get_prompt(request.name)
    }

    /// Mark the agent connected on initialize. This flag is the sole gate that
    /// lets hidden goal continuations chain (see the safety invariant in
    /// `session_runtime_driver.rs`): an agent that never opens this server can
    /// never reach `update_goal`, so it must not be allowed to self-chain.
    async fn initialize(
        &self,
        request: InitializeRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        let already_connected = self.shared.connected.swap(true, Ordering::Relaxed);
        if !already_connected {
            let update = capabilities_update(
                self.shared.agent_capabilities.clone(),
                self.shared.nori_client_advertised,
                true,
                self.shared.goal_ext_advertised,
                self.has_tool("get_goal") && self.has_tool("update_goal"),
            );
            let _ = self
                .shared
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    nori_protocol::NoriEvent::CapabilitiesChanged(public_nori_capabilities(update)),
                )))
                .await;
        }
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }
}

/// What every agent is told about the tools, however it reaches them.
pub(super) fn tool_instructions() -> String {
    format!(
        "{}\n\n{}\n\n{}\n\n{}",
        thread_goal::NORI_GOAL_CONTROL_INSTRUCTIONS,
        super::orchestrator::TASK_INSTRUCTIONS,
        nori_host_tools::MONITOR_INSTRUCTIONS,
        bash_tool::BASH_INSTRUCTIONS,
    )
}

pub(super) fn tool_success(body: serde_json::Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(body.to_string())])
}

pub(super) fn tool_error(message: impl Into<String>) -> CallToolResult {
    CallToolResult::error(vec![Content::text(message.into())])
}

#[cfg(test)]
pub(super) use goal_tools::CreateGoalRequest;
#[cfg(test)]
pub(super) use goal_tools::GoalCompletion;
#[cfg(test)]
pub(super) use goal_tools::UpdateGoalRequest;
pub(crate) use server::NoriClient;
#[cfg(test)]
pub(crate) use server::NoriClientServer;
pub(super) use server::PendingNoriClientServer;
pub(super) use server::reachable;
pub(super) use server::register_for_session;
pub use server::set_stdio_bridge;
pub(super) use session_capabilities::agent_capabilities_view;
use session_capabilities::capabilities_update;
pub(super) use session_capabilities::capabilities_update_for_nori_client;
pub(super) use session_capabilities::capabilities_update_for_session;
