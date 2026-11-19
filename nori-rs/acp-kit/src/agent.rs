//! The trait an in-process agent implements, and what it declares about itself.

use agent_client_protocol as acp;
use async_trait::async_trait;
use serde_json::Value;

use crate::schema::AuthMethod;
use crate::schema::AuthenticateRequest;
use crate::schema::AuthenticateResponse;
use crate::schema::CloseSessionRequest;
use crate::schema::CloseSessionResponse;
use crate::schema::DeleteSessionRequest;
use crate::schema::DeleteSessionResponse;
use crate::schema::ForkSessionRequest;
use crate::schema::ForkSessionResponse;
use crate::schema::InitializeRequest;
use crate::schema::ListSessionsRequest;
use crate::schema::ListSessionsResponse;
use crate::schema::LoadSessionRequest;
use crate::schema::LoadSessionResponse;
use crate::schema::LogoutRequest;
use crate::schema::LogoutResponse;
use crate::schema::Meta;
use crate::schema::NewSessionRequest;
use crate::schema::NewSessionResponse;
use crate::schema::PromptRequest;
use crate::schema::PromptResponse;
use crate::schema::ResumeSessionRequest;
use crate::schema::ResumeSessionResponse;
use crate::schema::SessionId;
use crate::schema::SetSessionConfigOptionRequest;
use crate::schema::SetSessionConfigOptionResponse;
use crate::schema::SetSessionModeRequest;
use crate::schema::SetSessionModeResponse;
use crate::sink::Cx;

/// What the agent can do. The kit refuses, with `method_not_found`, every request for a feature
/// the agent did not declare, so an agent cannot claim more than it implements and cannot
/// be asked for it either.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// Image blocks in a prompt.
    pub image: bool,
    /// Embedded resource blocks in a prompt.
    pub embedded_context: bool,
    /// MCP servers over HTTP in `session/new`.
    pub mcp_http: bool,
    /// MCP servers over SSE in `session/new`.
    pub mcp_sse: bool,
    /// `session/load`.
    pub load: bool,
    /// `session/resume`.
    pub resume: bool,
    /// `session/fork`.
    pub fork: bool,
    /// `session/list`.
    pub list: bool,
    /// `session/close`.
    pub close: bool,
    /// `session/delete`.
    pub delete: bool,
    /// `additionalDirectories` in session requests.
    pub additional_directories: bool,
    /// `logout`.
    pub logout: bool,
    /// The agent takes Nori's host tools (see `nori_host_tools`). Only an agent in Nori's own
    /// process may say so.
    pub host_tools: bool,
}

/// Who the agent is, answered once at `initialize`.
#[derive(Debug, Clone)]
pub struct Profile {
    /// The wire name, such as `nori-gemini-acp`.
    pub name: &'static str,
    /// What a person calls it.
    pub title: &'static str,
    pub version: &'static str,
    pub capabilities: Capabilities,
    pub auth_methods: Vec<AuthMethod>,
    /// Extra `_meta` for the initialize response. Host tools are advertised by the kit.
    pub meta: Option<Meta>,
}

impl Profile {
    pub fn new(name: &'static str, title: &'static str, version: &'static str) -> Self {
        Self {
            name,
            title,
            version,
            capabilities: Capabilities::default(),
            auth_methods: Vec::new(),
            meta: None,
        }
    }

    pub fn capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    pub fn auth_methods(mut self, methods: Vec<AuthMethod>) -> Self {
        self.auth_methods = methods;
        self
    }
}

/// An agent that runs in Nori's process. It implements only what is specific to its provider;
/// [`crate::serve`] owns the connection, the request routing, per-request tasks and the
/// end-of-connection shutdown.
///
/// A method that is not overridden answers [`crate::AgentError::Unsupported`], and the kit does
/// not call it unless the matching [`Capabilities`] flag is set. For an agent whose sessions
/// are plain values, the layer above, [`crate::Provider`], supplies the session table, the
/// one-prompt-at-a-time rule and cancellation as well.
#[async_trait]
pub trait NativeAgent: Send + Sync + 'static {
    /// Identity and capabilities.
    fn profile(&self) -> Profile;

    /// The client's `initialize` request, for an agent that needs the client's capabilities.
    fn on_initialize(&self, _request: &InitializeRequest) {}

    async fn authenticate(
        &self,
        _cx: &Cx,
        _request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        Ok(AuthenticateResponse::new())
    }

    async fn logout(&self, _cx: &Cx, _request: LogoutRequest) -> acp::Result<LogoutResponse> {
        Err(crate::AgentError::Unsupported("logout").into())
    }

    async fn new_session(
        &self,
        cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse>;

    async fn load_session(
        &self,
        _cx: &Cx,
        _request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        Err(crate::AgentError::Unsupported("session/load").into())
    }

    async fn resume_session(
        &self,
        _cx: &Cx,
        _request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        Err(crate::AgentError::Unsupported("session/resume").into())
    }

    async fn fork_session(
        &self,
        _cx: &Cx,
        _request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        Err(crate::AgentError::Unsupported("session/fork").into())
    }

    async fn list_sessions(
        &self,
        _cx: &Cx,
        _request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        Err(crate::AgentError::Unsupported("session/list").into())
    }

    async fn close_session(
        &self,
        _cx: &Cx,
        _request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        Err(crate::AgentError::Unsupported("session/close").into())
    }

    async fn delete_session(
        &self,
        _cx: &Cx,
        _request: DeleteSessionRequest,
    ) -> acp::Result<DeleteSessionResponse> {
        Err(crate::AgentError::Unsupported("session/delete").into())
    }

    async fn set_session_mode(
        &self,
        _cx: &Cx,
        _request: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        Err(crate::AgentError::Unsupported("session/set_mode").into())
    }

    async fn set_session_config_option(
        &self,
        _cx: &Cx,
        _request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        Err(crate::AgentError::Unsupported("session/set_config_option").into())
    }

    /// Run one prompt to its end. The response is the turn's result; its updates went out
    /// through `cx` as they happened.
    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse>;

    /// The client cancelled the running prompt of `session`. Cancelling an idle session is not an
    /// error.
    async fn cancel(&self, cx: &Cx, session: &SessionId);

    /// A request the kit has no typed route for, such as a vendor extension.
    async fn ext_request(&self, _cx: &Cx, _method: &str, _params: Value) -> acp::Result<Value> {
        Err(acp::Error::method_not_found())
    }

    /// A `session/new`, `load`, `resume` or `fork` succeeded and its response has been sent, so
    /// the session may now receive updates the client expects after that response (its slash
    /// commands, for instance).
    async fn session_ready(&self, _cx: &Cx, _session: &SessionId) {}

    /// The connection ended: stop whatever the agent started.
    async fn shutdown(&self) {}
}
