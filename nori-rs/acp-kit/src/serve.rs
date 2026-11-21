//! Serving a [`NativeAgent`] over an ACP connection.
//!
//! One place owns what every in-process agent used to repeat: the handler chain, a task per
//! request (so a long prompt never blocks `session/cancel`), refusing requests for capabilities
//! the agent did not declare, noticing that the client closed its end of the pipe, and shutting
//! the agent down afterwards.

use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use agent_client_protocol as acp;
use agent_client_protocol::Agent;
use agent_client_protocol::ByteStreams;
use agent_client_protocol::ConnectTo;
use agent_client_protocol::Dispatch;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;

use crate::agent::NativeAgent;
use crate::agent::Profile;
use crate::registration::Host;
use crate::schema::AgentAuthCapabilities;
use crate::schema::AgentCapabilities;
use crate::schema::AuthenticateRequest;
use crate::schema::CancelNotification;
use crate::schema::CloseSessionRequest;
use crate::schema::DeleteSessionRequest;
use crate::schema::ForkSessionRequest;
use crate::schema::ForkSessionResponse;
use crate::schema::Implementation;
use crate::schema::InitializeRequest;
use crate::schema::InitializeResponse;
use crate::schema::ListSessionsRequest;
use crate::schema::LoadSessionRequest;
use crate::schema::LoadSessionResponse;
use crate::schema::LogoutCapabilities;
use crate::schema::LogoutRequest;
use crate::schema::McpCapabilities;
use crate::schema::NewSessionRequest;
use crate::schema::NewSessionResponse;
use crate::schema::PromptCapabilities;
use crate::schema::PromptRequest;
use crate::schema::ResumeSessionRequest;
use crate::schema::ResumeSessionResponse;
use crate::schema::SessionAdditionalDirectoriesCapabilities;
use crate::schema::SessionCapabilities;
use crate::schema::SessionCloseCapabilities;
use crate::schema::SessionDeleteCapabilities;
use crate::schema::SessionForkCapabilities;
use crate::schema::SessionId;
use crate::schema::SessionListCapabilities;
use crate::schema::SessionResumeCapabilities;
use crate::schema::SetSessionConfigOptionRequest;
use crate::schema::SetSessionModeRequest;
use crate::sink::Cx;

/// Route one request type to the agent method of that name, on a task of its own. A request for a
/// feature the agent did not declare is refused without reaching it.
macro_rules! route {
    ($builder:expr, $agent:ident, $request:ty, $method:ident, $allowed:expr) => {
        $builder.on_receive_request(
            {
                let agent = Arc::clone(&$agent);
                async move |request: $request, responder, cx: Cx| {
                    let agent = Arc::clone(&agent);
                    let client = cx.clone();
                    cx.spawn(async move {
                        let result = if $allowed {
                            agent.$method(&client, request).await
                        } else {
                            Err(acp::Error::method_not_found())
                        };
                        responder.respond_with_result(result)
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
    };
}

/// [`route!`] for a request that opens a session. After the response is sent, the agent hears
/// [`NativeAgent::session_ready`] for the session `$announce` names.
macro_rules! route_opening {
    ($builder:expr, $agent:ident, $request:ty, $response:ty, $method:ident, $allowed:expr, $announce:expr) => {
        $builder.on_receive_request(
            {
                let agent = Arc::clone(&$agent);
                async move |request: $request, responder, cx: Cx| {
                    let agent = Arc::clone(&agent);
                    let client = cx.clone();
                    cx.spawn(async move {
                        let announce: fn(&$request, &$response) -> Option<SessionId> = $announce;
                        let mut opened = None;
                        let result = if $allowed {
                            let announced = request.clone();
                            let result = agent.$method(&client, request).await;
                            opened = result
                                .as_ref()
                                .ok()
                                .and_then(|response| announce(&announced, response));
                            result
                        } else {
                            Err(acp::Error::method_not_found())
                        };
                        responder.respond_with_result(result)?;
                        if let Some(session) = opened {
                            agent.session_ready(&client, &session).await;
                        }
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_request!(),
        )
    };
}

/// Serve `agent` over one end of an in-memory pipe until the client closes the other end.
/// The host keeps the other end, so it needs none of this crate's protocol types, only `tokio`.
pub async fn serve_over_pipe<A: NativeAgent>(
    pipe: tokio::io::DuplexStream,
    agent: A,
) -> std::io::Result<()> {
    serve_over_pipe_as(pipe, agent, Host::Nori).await
}

/// [`serve_over_pipe`] for a client that is not Nori's own harness (see [`Host`]).
pub async fn serve_over_pipe_as<A: NativeAgent>(
    pipe: tokio::io::DuplexStream,
    agent: A,
    host: Host,
) -> std::io::Result<()> {
    let (read, write) = tokio::io::split(pipe);
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    let incoming = EofRead {
        inner: read,
        signal: Some(eof_tx),
    };
    serve_until(
        ByteStreams::new(write.compat_write(), incoming.compat()),
        agent,
        host,
        eof_rx,
    )
    .await
}

/// Serve `agent` over any transport, until the transport ends. The client is taken to be
/// [`Host::External`].
pub async fn serve<A: NativeAgent>(
    transport: impl ConnectTo<Agent> + 'static,
    agent: A,
) -> std::io::Result<()> {
    serve_until(
        transport,
        agent,
        Host::External,
        std::future::pending::<()>(),
    )
    .await
}

async fn serve_until<A: NativeAgent>(
    transport: impl ConnectTo<Agent> + 'static,
    agent: A,
    host: Host,
    stop: impl std::future::Future<Output = impl Sized>,
) -> std::io::Result<()> {
    let agent = Arc::new(agent);
    agent.on_host(host);
    let mut profile = agent.profile();
    if host == Host::External {
        // Host tool tokens mean nothing outside Nori's own harness.
        profile.capabilities.host_tools = false;
    }
    let profile = Arc::new(profile);
    let caps = profile.capabilities;
    let builder = Agent.builder().name(profile.name);

    let builder = builder.on_receive_request(
        {
            let agent = Arc::clone(&agent);
            let profile = Arc::clone(&profile);
            async move |request: InitializeRequest, responder, _cx| {
                agent.on_initialize(&request);
                responder.respond(initialize_response(&profile, &request))
            }
        },
        acp::on_receive_request!(),
    );
    let builder = route!(builder, agent, AuthenticateRequest, authenticate, true);
    let builder = route!(builder, agent, LogoutRequest, logout, caps.logout);
    let builder = route_opening!(
        builder,
        agent,
        NewSessionRequest,
        NewSessionResponse,
        new_session,
        true,
        |_, response| Some(response.session_id.clone())
    );
    let builder = route_opening!(
        builder,
        agent,
        LoadSessionRequest,
        LoadSessionResponse,
        load_session,
        caps.load,
        |request, _| Some(request.session_id.clone())
    );
    let builder = route_opening!(
        builder,
        agent,
        ResumeSessionRequest,
        ResumeSessionResponse,
        resume_session,
        caps.resume,
        |request, _| Some(request.session_id.clone())
    );
    let builder = route_opening!(
        builder,
        agent,
        ForkSessionRequest,
        ForkSessionResponse,
        fork_session,
        caps.fork,
        |_, response| Some(response.session_id.clone())
    );
    let builder = route!(
        builder,
        agent,
        ListSessionsRequest,
        list_sessions,
        caps.list
    );
    let builder = route!(
        builder,
        agent,
        CloseSessionRequest,
        close_session,
        caps.close
    );
    let builder = route!(
        builder,
        agent,
        DeleteSessionRequest,
        delete_session,
        caps.delete
    );
    let builder = route!(
        builder,
        agent,
        SetSessionModeRequest,
        set_session_mode,
        true
    );
    let builder = route!(
        builder,
        agent,
        SetSessionConfigOptionRequest,
        set_session_config_option,
        true
    );
    let builder = route!(builder, agent, PromptRequest, prompt, true);
    let serving = builder
        .on_receive_notification(
            {
                let agent = Arc::clone(&agent);
                async move |notification: CancelNotification, cx: Cx| {
                    let agent = Arc::clone(&agent);
                    let client = cx.clone();
                    cx.spawn(async move {
                        agent.cancel(&client, &notification.session_id).await;
                        Ok(())
                    })?;
                    Ok(())
                }
            },
            acp::on_receive_notification!(),
        )
        .on_receive_dispatch(
            {
                let agent = Arc::clone(&agent);
                async move |message: Dispatch, cx: Cx| match message {
                    Dispatch::Response(result, router) => router.respond_with_result(result),
                    Dispatch::Request(request, responder) => {
                        let agent = Arc::clone(&agent);
                        let client = cx.clone();
                        cx.spawn(async move {
                            let result = agent
                                .ext_request(&client, &request.method, request.params)
                                .await;
                            responder.respond_with_result(result)
                        })?;
                        Ok(())
                    }
                    Dispatch::Notification(_) => Ok(()),
                }
            },
            acp::on_receive_dispatch!(),
        )
        .connect_to(transport);
    let result = tokio::select! {
        result = serving => result.map_err(|error| std::io::Error::other(error.to_string())),
        _ = stop => Ok(()),
    };
    agent.shutdown().await;
    result
}

fn initialize_response(profile: &Profile, request: &InitializeRequest) -> InitializeResponse {
    let caps = profile.capabilities;
    let mut sessions = SessionCapabilities::new();
    if caps.list {
        sessions = sessions.list(SessionListCapabilities::new());
    }
    if caps.fork {
        sessions = sessions.fork(SessionForkCapabilities::new());
    }
    if caps.resume {
        sessions = sessions.resume(SessionResumeCapabilities::new());
    }
    if caps.close {
        sessions = sessions.close(SessionCloseCapabilities::new());
    }
    if caps.delete {
        sessions = sessions.delete(SessionDeleteCapabilities::new());
    }
    if caps.additional_directories {
        sessions = sessions.additional_directories(SessionAdditionalDirectoriesCapabilities::new());
    }
    let mut capabilities = AgentCapabilities::new()
        .load_session(caps.load)
        .session_capabilities(sessions)
        .mcp_capabilities(McpCapabilities::new().http(caps.mcp_http).sse(caps.mcp_sse))
        .prompt_capabilities(
            PromptCapabilities::new()
                .image(caps.image)
                .embedded_context(caps.embedded_context),
        );
    if caps.logout {
        capabilities =
            capabilities.auth(AgentAuthCapabilities::new().logout(LogoutCapabilities::new()));
    }
    let mut meta = profile.meta.clone().unwrap_or_default();
    if caps.host_tools {
        nori_host_tools::advertise(&mut meta);
    }
    let response = InitializeResponse::new(request.protocol_version)
        .agent_info(Implementation::new(profile.name, profile.version).title(profile.title))
        .agent_capabilities(capabilities)
        .auth_methods(profile.auth_methods.clone());
    if meta.is_empty() {
        response
    } else {
        response.meta(meta)
    }
}

/// A reader that reports once the client has closed its end.
struct EofRead<R> {
    inner: R,
    signal: Option<tokio::sync::oneshot::Sender<()>>,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for EofRead<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let has_space = buffer.remaining() > 0;
        let result = Pin::new(&mut self.inner).poll_read(cx, buffer);
        if matches!(result, Poll::Ready(Ok(())))
            && has_space
            && buffer.filled().len() == before
            && let Some(signal) = self.signal.take()
        {
            let _ = signal.send(());
        }
        result
    }
}
