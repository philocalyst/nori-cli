//! [`ClaudeAcpAgent`] as a kit agent.

use nori_acp_kit::Capabilities;
use nori_acp_kit::Cx;
use nori_acp_kit::Host;
use nori_acp_kit::NativeAgent;
use nori_acp_kit::Profile;
use nori_acp_kit::acp;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::CloseSessionRequest;
use nori_acp_kit::schema::CloseSessionResponse;
use nori_acp_kit::schema::ForkSessionRequest;
use nori_acp_kit::schema::ForkSessionResponse;
use nori_acp_kit::schema::InitializeRequest;
use nori_acp_kit::schema::ListSessionsRequest;
use nori_acp_kit::schema::ListSessionsResponse;
use nori_acp_kit::schema::LoadSessionRequest;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::NewSessionRequest;
use nori_acp_kit::schema::NewSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::PromptResponse;
use nori_acp_kit::schema::ResumeSessionRequest;
use nori_acp_kit::schema::ResumeSessionResponse;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SetSessionConfigOptionRequest;
use nori_acp_kit::schema::SetSessionConfigOptionResponse;
use nori_acp_kit::schema::SetSessionModeRequest;
use nori_acp_kit::schema::SetSessionModeResponse;

use super::ClaudeAcpAgent;
use super::catalog;
use super::lifecycle;
use super::listing;
use super::prompt;
use crate::types::AgentError;

#[async_trait]
impl NativeAgent for ClaudeAcpAgent {
    fn profile(&self) -> Profile {
        Profile::new("nori-claude-acp", "Claude Code", env!("CARGO_PKG_VERSION")).capabilities(
            Capabilities {
                image: true,
                embedded_context: true,
                mcp_http: true,
                mcp_sse: true,
                load: true,
                resume: true,
                fork: true,
                list: true,
                close: true,
                host_tools: true,
                ..Capabilities::default()
            },
        )
    }

    fn on_host(&self, host: Host) {
        self.nori_host
            .store(host == Host::Nori, std::sync::atomic::Ordering::Relaxed);
    }

    /// The client's capabilities decide which of its methods the session's tools may call.
    fn on_initialize(&self, request: &InitializeRequest) {
        self.sessions
            .set_client_terminal(request.client_capabilities.terminal);
    }

    async fn new_session(
        &self,
        cx: &Cx,
        request: NewSessionRequest,
    ) -> acp::Result<NewSessionResponse> {
        if self.nori_host.load(std::sync::atomic::Ordering::Relaxed)
            && !self
                .identity_announced
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            super::identity::announce(cx, &self.config);
        }
        Ok(lifecycle::new_session(request, &self.config, &self.sessions, cx).await?)
    }

    async fn load_session(
        &self,
        cx: &Cx,
        request: LoadSessionRequest,
    ) -> acp::Result<LoadSessionResponse> {
        Ok(lifecycle::load_session(request, &self.config, &self.sessions, cx).await?)
    }

    async fn resume_session(
        &self,
        cx: &Cx,
        request: ResumeSessionRequest,
    ) -> acp::Result<ResumeSessionResponse> {
        Ok(lifecycle::resume_session(request, &self.config, &self.sessions, cx).await?)
    }

    async fn fork_session(
        &self,
        cx: &Cx,
        request: ForkSessionRequest,
    ) -> acp::Result<ForkSessionResponse> {
        Ok(lifecycle::fork_session(request, &self.config, &self.sessions, cx).await?)
    }

    async fn list_sessions(
        &self,
        _cx: &Cx,
        request: ListSessionsRequest,
    ) -> acp::Result<ListSessionsResponse> {
        Ok(listing::list_sessions(request, &self.config).await?)
    }

    /// Release a session: its running turn is cancelled, and its CLI, the CLI's tool servers and
    /// its background shells are stopped. The conversation stays on disk, so it can be loaded
    /// or resumed later. Closing a session this agent does not hold is not an error.
    async fn close_session(
        &self,
        _cx: &Cx,
        request: CloseSessionRequest,
    ) -> acp::Result<CloseSessionResponse> {
        let id = request.session_id.0.as_ref();
        if let Some(session) = self.sessions.get_session(id) {
            session.cancel().await;
        }
        self.sessions.remove_and_cleanup(id).await?;
        Ok(CloseSessionResponse::new())
    }

    async fn set_session_mode(
        &self,
        cx: &Cx,
        request: SetSessionModeRequest,
    ) -> acp::Result<SetSessionModeResponse> {
        catalog::set_mode(
            &self.sessions,
            cx,
            &request.session_id,
            request.mode_id.0.as_ref(),
        )
        .await?;
        Ok(SetSessionModeResponse::new())
    }

    async fn set_session_config_option(
        &self,
        cx: &Cx,
        request: SetSessionConfigOptionRequest,
    ) -> acp::Result<SetSessionConfigOptionResponse> {
        let value = request
            .value
            .as_value_id()
            .ok_or_else(|| AgentError::invalid("option values are selections"))?
            .to_string();
        let session = self
            .sessions
            .get_session_or_error(request.session_id.0.as_ref())?;
        match request.config_id.0.as_ref() {
            nori_acp_kit::MODE => {
                catalog::set_mode(&self.sessions, cx, &request.session_id, &value).await?;
            }
            nori_acp_kit::MODEL => session.set_model(value).await,
            catalog::FAST => match value.as_str() {
                "on" => session.set_fast_mode(true),
                "off" => session.set_fast_mode(false),
                other => {
                    return Err(AgentError::invalid(format!("invalid fast mode {other}")).into());
                }
            },
            other => {
                return Err(AgentError::invalid(format!("unknown config option {other}")).into());
            }
        }
        Ok(SetSessionConfigOptionResponse::new(
            catalog::session_options(&session).await,
        ))
    }

    async fn prompt(&self, cx: &Cx, request: PromptRequest) -> acp::Result<PromptResponse> {
        Ok(prompt::run(request, &self.sessions, cx).await?)
    }

    async fn cancel(&self, _cx: &Cx, session: &SessionId) {
        match self.sessions.get_session_or_error(session.0.as_ref()) {
            Ok(session) => session.cancel().await,
            Err(error) => tracing::debug!(%error, "Cancel for a session that is gone"),
        }
    }

    async fn shutdown(&self) {
        self.sessions.clear().await;
    }
}
