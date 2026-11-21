//! ACP connection layer built on the `agent-client-protocol` SDK.
//!
//! The SDK's `ConnectionTo<Agent>` is `Send + Sync`, so all operations run
//! directly on the main tokio runtime without a dedicated worker thread or
//! `LocalSet`.

use std::path::Path;
use std::process::Stdio;

use agent_client_protocol::Agent;
use agent_client_protocol::Client;
use agent_client_protocol::ConnectionTo;
use agent_client_protocol::Dispatch;
use agent_client_protocol::HandleDispatchFrom;
use agent_client_protocol::Handled;
use agent_client_protocol::Lines;
use agent_client_protocol::UntypedMessage;
use anyhow::Context;
use anyhow::Result;
use futures::AsyncBufReadExt;
use futures::AsyncWriteExt;
use futures::StreamExt;
use futures::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::compat::TokioAsyncReadCompatExt;
use tokio_util::compat::TokioAsyncWriteCompatExt;
use tracing::debug;
use tracing::warn;

use super::AcpSessionConfigState;
use super::ConnectionEvent;
use super::DelegatedRequest;
use super::child_lifecycle::ChildHandle;
use super::child_lifecycle::SHUTDOWN_GRACE;
use super::child_lifecycle::STDERR_TAIL_LINES;
use super::child_lifecycle::collect_stderr_tail;
use super::child_lifecycle::wait_for_child_exit;
use super::child_registry::Routed;
use super::child_registry::SharedChildren;
use super::wire_log::WireDirection;
use super::wire_log::WireLogger;
use crate::registry::AcpAgentConfig;
use nori_config::AcpProxyConfig;
use nori_protocol::AcpEvent;
use nori_protocol::acp::ProtocolVersion;
use nori_protocol::acp::v1 as acp;
use nori_protocol::acp::v1::RequestId;

/// Minimum supported ACP protocol version.
const MINIMUM_SUPPORTED_VERSION: ProtocolVersion = ProtocolVersion::V1;

fn schema_request_id(id: serde_json::Value) -> RequestId {
    match id {
        serde_json::Value::Null => RequestId::Null,
        serde_json::Value::Number(number) => number
            .as_i64()
            .map(RequestId::Number)
            .unwrap_or_else(|| RequestId::Str(number.to_string())),
        serde_json::Value::String(id) => RequestId::Str(id),
        other => RequestId::Str(other.to_string()),
    }
}

/// A thread-safe connection to an ACP agent subprocess.
///
/// The `ConnectionTo<Agent>` from the `agent-client-protocol` SDK is
/// `Send + Sync`, so all operations run directly on the main tokio runtime
/// without a dedicated worker thread.
///
/// Internal architecture:
/// - A background tokio task runs the ACP connection via `connect_with`.
/// - The `ConnectionTo<Agent>` is cloned out and used for all subsequent requests.
/// - Session notifications and approval requests are forwarded via channels.
/// - All session-domain traffic flows through a single ordered inbox.
pub struct AcpConnection {
    /// Connection context for sending requests to the agent.
    cx: ConnectionTo<Agent>,

    /// Agent capabilities from the initialization handshake.
    agent_capabilities: acp::AgentCapabilities,

    /// Top-level `_meta` of the initialize response, where agents advertise
    /// ACP extensions (e.g. the `_session/goal` goal extension).
    initialize_meta: Option<acp::Meta>,

    /// `_meta` sent with every session request (new, load, resume, fork): how a built-in agent is
    /// handed the session's host tools.
    session_meta: std::sync::RwLock<Option<acp::Meta>>,

    /// Whether the agent is a built-in one, running in this process.
    in_process: bool,

    /// Ordered inbox of raw ACP events from the transport layer.
    event_rx: mpsc::Receiver<ConnectionEvent>,

    /// Which sessions on this connection are subagents of the root session.
    children: SharedChildren,

    /// Sender retained so outgoing request completions join the ordered inbox.
    event_tx: mpsc::Sender<ConnectionEvent>,

    /// Thread-safe session config state, updated from complete ACP snapshots.
    session_config_state: std::sync::Arc<std::sync::RwLock<AcpSessionConfigState>>,

    /// Handle to the background task driving the ACP connection.
    connection_task: tokio::task::JoinHandle<()>,

    /// Handles to the child process for teardown (None for remote connections).
    child: Option<ChildHandle>,

    /// Handle to the stderr logging task (None for remote connections).
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

/// How long a process whose output closed is given to exit by itself before it is stopped.
const OUTPUT_CLOSED_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// How a connection is set up beyond its transport.
struct ConnectionOptions {
    advertise_subagents: bool,
    /// The slug of the agent this connection was started for: who an `_auth/status_update`
    /// on it is about.
    agent_slug: String,
    children: SharedChildren,
    /// A subprocess agent: notice that its output closed while it lives.
    output_watch: Option<OutputWatch>,
}

/// What the connection needs to notice a live process whose output closed.
struct OutputWatch {
    exit_rx: tokio::sync::watch::Receiver<Option<std::process::ExitStatus>>,
    kill: std::sync::Arc<tokio::sync::Notify>,
    /// Fires when the agent's standard output reaches its end.
    closed: oneshot::Receiver<()>,
}

/// Claims the `session/update` notifications that announce subagents.
///
/// The pinned ACP schema has no types for them, so the typed notification handler cannot parse
/// them. They are recognized from the already-decoded JSON envelope (an O(1) peek at
/// `params.update.sessionUpdate`) and everything else is passed on untouched.
struct SubagentDispatch {
    event_tx: mpsc::Sender<ConnectionEvent>,
    children: SharedChildren,
}

impl HandleDispatchFrom<Agent> for SubagentDispatch {
    async fn handle_dispatch_from(
        &mut self,
        message: Dispatch,
        _connection: ConnectionTo<Agent>,
    ) -> std::result::Result<Handled<Dispatch>, acp::Error> {
        let Dispatch::Notification(notification) = message else {
            return Ok(Handled::No {
                message,
                retry: false,
            });
        };
        if notification.method != "session/update" {
            return Ok(Handled::No {
                message: Dispatch::Notification(notification),
                retry: false,
            });
        }
        let Some(wire) = nori_protocol::parse_subagent_update(&notification.params) else {
            return Ok(Handled::No {
                message: Dispatch::Notification(notification),
                retry: false,
            });
        };
        // Announcing a child releases the updates that arrived ahead of the announcement.
        let released = match &wire {
            nori_protocol::SubagentWire::Update(update) => self
                .children
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .announce(&update.session_id),
            nori_protocol::SubagentWire::Message(_) => Vec::new(),
        };
        if self
            .event_tx
            .send(ConnectionEvent::Subagent(wire))
            .await
            .is_err()
        {
            warn!("Notification channel closed, dropping subagent update");
        }
        for notification in released {
            let _ = self
                .event_tx
                .send(ConnectionEvent::Acp(Box::new(AcpEvent::ChildNotification(
                    notification,
                ))))
                .await;
        }
        Ok(Handled::Yes)
    }

    fn describe_chain(&self) -> impl std::fmt::Debug {
        "subagent updates"
    }
}

/// The `initialize` request as JSON, with `clientCapabilities.subagents = {}` when advertised.
///
/// The typed request cannot carry the capability, so it is serialized and extended here and sent
/// as an untyped message; the response is decoded as the usual typed response.
fn initialize_params(
    request: &acp::InitializeRequest,
    advertise_subagents: bool,
) -> std::result::Result<serde_json::Value, acp::Error> {
    let mut params = serde_json::to_value(request)?;
    if advertise_subagents
        && let Some(capabilities) = params
            .get_mut("clientCapabilities")
            .and_then(serde_json::Value::as_object_mut)
    {
        capabilities.insert("subagents".to_string(), serde_json::json!({}));
    }
    Ok(params)
}

/// Write a file for the agent, if the path is inside the working directory or `/tmp`.
fn write_text_file(cwd: &Path, requested: &Path, content: &str) -> Result<(), acp::Error> {
    let resolved_path = if requested.is_relative() {
        cwd.join(requested)
    } else {
        requested.to_path_buf()
    };

    let allowed = if let Ok(canonical) = resolved_path.canonicalize() {
        let in_cwd = cwd
            .canonicalize()
            .map(|c| canonical.starts_with(&c))
            .unwrap_or(false);
        let in_tmp = canonical.starts_with("/tmp");
        in_cwd || in_tmp
    } else if let Some(parent) = resolved_path.parent() {
        if let Ok(canonical_parent) = parent.canonicalize() {
            let in_cwd = cwd
                .canonicalize()
                .map(|c| canonical_parent.starts_with(&c))
                .unwrap_or(false);
            let in_tmp = canonical_parent.starts_with("/tmp");
            in_cwd || in_tmp
        } else {
            resolved_path.starts_with(cwd) || resolved_path.starts_with("/tmp")
        }
    } else {
        false
    };

    if !allowed {
        return Err(acp::Error::invalid_params().data(format!(
            "Write restricted to working directory ({}) or /tmp. Path: {}",
            cwd.display(),
            resolved_path.display()
        )));
    }

    if let Some(parent) = resolved_path.parent()
        && !parent.exists()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return Err(acp::Error::internal_error().data(e.to_string()));
    }

    std::fs::write(&resolved_path, content)
        .map_err(|e| acp::Error::internal_error().data(e.to_string()))
}

/// Size of the in-memory pipe between Nori and a built-in agent. Writes wait when it is full.
const IN_PROCESS_PIPE_BYTES: usize = 1 << 20;

/// Shared ACP connection setup: registers notification/request handlers,
/// performs the initialization handshake, and returns a `AcpConnection` with
/// no process handles (`child`/`stderr_task` are `None`). `spawn()` fills in the
/// subprocess handles afterward.
///
/// The caller supplies the connection event channel so it can hold extra
/// senders (e.g. the child exit watcher reports through the same stream).
async fn establish_connection(
    transport: impl agent_client_protocol::ConnectTo<Client> + 'static,
    cwd: &Path,
    event_tx: mpsc::Sender<ConnectionEvent>,
    event_rx: mpsc::Receiver<ConnectionEvent>,
    initialize_event_tx: Option<mpsc::Sender<AcpEvent>>,
    options: ConnectionOptions,
) -> Result<AcpConnection> {
    let ConnectionOptions {
        advertise_subagents,
        agent_slug,
        children,
        output_watch,
    } = options;
    let connection_event_tx = event_tx.clone();
    let event_tx_for_closed = event_tx.clone();
    let event_tx_for_notifications = event_tx.clone();
    let event_tx_for_initialize = event_tx.clone();
    let session_config_state =
        std::sync::Arc::new(std::sync::RwLock::new(AcpSessionConfigState::new()));
    let session_config_state_for_notifications = session_config_state.clone();
    let write_cwd = cwd.to_path_buf();
    let read_cwd = cwd.to_path_buf();

    let (init_tx, init_rx) = oneshot::channel::<
        Result<(
            ConnectionTo<Agent>,
            acp::AgentCapabilities,
            Option<acp::Meta>,
        )>,
    >();

    let subagent_event_tx = event_tx.clone();
    let children_for_dispatch = children.clone();
    let children_for_notifications = children.clone();
    let connection_task = tokio::spawn(async move {
        let result = Client
            .builder()
            .with_handler(SubagentDispatch {
                event_tx: subagent_event_tx,
                children: children_for_dispatch,
            })
            .on_receive_notification(
                {
                    let event_tx = event_tx_for_notifications;
                    let session_config_state = session_config_state_for_notifications;
                    let children = children_for_notifications;
                    async move |notification: acp::SessionNotification, _connection| {
                        let routed = children
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .route(notification);
                        let notification = match routed {
                            Routed::Root(notification) => notification,
                            Routed::Child(notification) => {
                                if event_tx
                                    .send(ConnectionEvent::Acp(Box::new(
                                        AcpEvent::ChildNotification(notification),
                                    )))
                                    .await
                                    .is_err()
                                {
                                    warn!("Notification channel closed, dropping child update");
                                }
                                return Ok(());
                            }
                            Routed::Held => return Ok(()),
                        };
                        let session_id = notification.session_id.to_string();
                        // One per streamed chunk: trace, so a long reply is not a long log.
                        tracing::trace!(
                            target: "acp_event_flow",
                            session_id,
                            update_kind = super::session_update_kind(&notification.update),
                            "Transport received ACP session/update notification"
                        );
                        if let acp::SessionUpdate::ConfigOptionUpdate(update) = &notification.update
                            && let Ok(mut state) = session_config_state.write()
                        {
                            state.config_options = update.config_options.clone();
                        }
                        if event_tx
                            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Notification(
                                acp::AgentNotification::SessionNotification(notification.clone()),
                            ))))
                            .await
                            .is_err()
                        {
                            warn!("Notification channel closed, dropping notification");
                            return Ok(());
                        }
                        if event_tx
                            .send(ConnectionEvent::SessionUpdate(notification.update))
                            .await
                            .is_err()
                        {
                            warn!("Notification channel closed, dropping reducer update");
                        }
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_notification(
                {
                    let event_tx = event_tx.clone();
                    let agent_slug = agent_slug.clone();
                    async move |notification: UntypedMessage, _connection: ConnectionTo<Agent>| {
                        if notification.method() == nori_acp_kit::AUTH_STATUS_UPDATE {
                            if let Some(status) =
                                nori_acp_kit::AuthStatus::from_params(notification.params())
                            {
                                crate::accounts::record_identity(&agent_slug, &status);
                            }
                            return Ok(());
                        }
                        if notification.method() == "_nori/user_input_cancelled" {
                            let raw = serde_json::value::RawValue::from_string(
                                notification.params().to_string(),
                            )
                            .map_err(|error| {
                                acp::Error::invalid_params().data(error.to_string())
                            })?;
                            let event = nori_protocol::AcpEvent::Notification(
                                acp::AgentNotification::ExtNotification(acp::ExtNotification::new(
                                    notification.method(),
                                    raw.into(),
                                )),
                            );
                            let _ = event_tx.send(ConnectionEvent::Acp(Box::new(event))).await;
                        }
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .on_receive_request(
                {
                    let event_tx = event_tx.clone();
                    async move |request: acp::RequestPermissionRequest,
                                responder: agent_client_protocol::Responder<
                        acp::RequestPermissionResponse,
                    >,
                                connection: ConnectionTo<Agent>| {
                        let (response_tx, response_rx) = oneshot::channel();
                        let delegated = DelegatedRequest {
                            request_id: schema_request_id(responder.id()),
                            request: acp::AgentRequest::RequestPermissionRequest(request),
                            response_tx,
                        };

                        if event_tx
                            .send(ConnectionEvent::DelegatedRequest(delegated))
                            .await
                            .is_err()
                        {
                            responder.respond(acp::RequestPermissionResponse::new(
                                acp::RequestPermissionOutcome::Cancelled,
                            ))?;
                            return Ok(());
                        }

                        connection.spawn(async move {
                            let response = match response_rx.await {
                                Ok(Ok(acp::ClientResponse::RequestPermissionResponse(
                                    response,
                                ))) => Ok(response),
                                Ok(Ok(_)) => Err(acp::Error::invalid_params()
                                    .data("response variant does not match permission request")),
                                Ok(Err(error)) => Err(error),
                                Err(_) => Ok(acp::RequestPermissionResponse::new(
                                    acp::RequestPermissionOutcome::Cancelled,
                                )),
                            };
                            responder.respond_with_result(response)?;
                            Ok(())
                        })?;

                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let cwd = write_cwd;
                    async move |request: acp::WriteTextFileRequest,
                                responder: agent_client_protocol::Responder<
                        acp::WriteTextFileResponse,
                    >,
                                connection: ConnectionTo<Agent>| {
                        // File access runs off the connection's dispatch loop: a large read or
                        // write must not hold up the agent's streamed output behind it.
                        let cwd = cwd.clone();
                        connection.spawn(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                write_text_file(&cwd, &request.path, &request.content)
                            })
                            .await
                            .unwrap_or_else(|error| {
                                Err(acp::Error::internal_error().data(error.to_string()))
                            });
                            responder.respond_with_result(
                                result.map(|()| acp::WriteTextFileResponse::new()),
                            )?;
                            Ok(())
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let cwd = read_cwd;
                    async move |request: acp::ReadTextFileRequest,
                                responder: agent_client_protocol::Responder<
                        acp::ReadTextFileResponse,
                    >,
                                connection: ConnectionTo<Agent>| {
                        let resolved_path = if request.path.is_relative() {
                            cwd.join(&request.path)
                        } else {
                            request.path
                        };
                        connection.spawn(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                std::fs::read_to_string(&resolved_path)
                            })
                            .await
                            .unwrap_or_else(|error| Err(std::io::Error::other(error)));
                            responder.respond_with_result(
                                result.map(acp::ReadTextFileResponse::new).map_err(|error| {
                                    acp::Error::internal_error().data(error.to_string())
                                }),
                            )?;
                            Ok(())
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                {
                    let event_tx = event_tx.clone();
                    async move |request: UntypedMessage,
                                responder: agent_client_protocol::Responder<serde_json::Value>,
                                connection: ConnectionTo<Agent>| {
                        if request.method() != "_nori/request_user_input" {
                            responder.respond_with_error(acp::Error::method_not_found())?;
                            return Ok(());
                        }
                        let raw =
                            serde_json::value::RawValue::from_string(request.params().to_string())
                                .map_err(|error| {
                                    acp::Error::invalid_params().data(error.to_string())
                                })?;
                        let (response_tx, response_rx) = oneshot::channel();
                        let delegated = DelegatedRequest {
                            request_id: schema_request_id(responder.id()),
                            request: acp::AgentRequest::ExtMethodRequest(acp::ExtRequest::new(
                                request.method(),
                                raw.into(),
                            )),
                            response_tx,
                        };
                        if event_tx
                            .send(ConnectionEvent::DelegatedRequest(delegated))
                            .await
                            .is_err()
                        {
                            responder.respond_with_error(
                                acp::Error::internal_error().data("question host disconnected"),
                            )?;
                            return Ok(());
                        }
                        connection.spawn(async move {
                            let response = match response_rx.await {
                                Ok(Ok(acp::ClientResponse::ExtMethodResponse(response))) => {
                                    serde_json::from_str(response.0.get()).map_err(|error| {
                                        acp::Error::internal_error().data(error.to_string())
                                    })
                                }
                                Ok(Ok(_)) => Err(acp::Error::invalid_params()
                                    .data("response variant does not match user question")),
                                Ok(Err(error)) => Err(error),
                                Err(_) => Err(acp::Error::internal_error()
                                    .data("question response channel closed")),
                            };
                            responder.respond_with_result(response)?;
                            Ok(())
                        })?;
                        Ok(())
                    }
                },
                agent_client_protocol::on_receive_request!(),
            )
            .connect_with(transport, |connection: ConnectionTo<Agent>| async move {
                let initialize = acp::InitializeRequest::new(ProtocolVersion::LATEST)
                    .client_capabilities(
                        acp::ClientCapabilities::new()
                            .fs(acp::FileSystemCapabilities::new()
                                .read_text_file(true)
                                .write_text_file(true))
                            // Nori runs an agent's terminal sign-in itself (`sign_in`).
                            .auth(acp::AuthCapabilities::new().terminal(true)),
                    )
                    .client_info(
                        acp::Implementation::new("nori", env!("CARGO_PKG_VERSION"))
                            .title("Nori CLI"),
                    );
                let request = connection.send_request(UntypedMessage::new(
                    "initialize",
                    initialize_params(&initialize, advertise_subagents)?,
                )?);
                let request_id = schema_request_id(request.id());
                let response = request.block_task().await.and_then(|value| {
                    serde_json::from_value::<acp::InitializeResponse>(value)
                        .map_err(acp::Error::from)
                });
                let initialize_event = AcpEvent::Response {
                    request_id,
                    response: response.clone().map(acp::AgentResponse::InitializeResponse),
                };
                if let Some(initialize_event_tx) = initialize_event_tx.as_ref() {
                    if let Err(error) = initialize_event_tx.send(initialize_event).await {
                        let _ = event_tx_for_initialize
                            .send(ConnectionEvent::Acp(Box::new(error.0)))
                            .await;
                    }
                } else {
                    let _ = event_tx_for_initialize
                        .send(ConnectionEvent::Acp(Box::new(initialize_event)))
                        .await;
                }

                match response {
                    Ok(resp) => {
                        if resp.protocol_version < MINIMUM_SUPPORTED_VERSION {
                            let _ = init_tx.send(Err(anyhow::anyhow!(
                                "ACP agent version {} is too old (minimum: {})",
                                resp.protocol_version,
                                MINIMUM_SUPPORTED_VERSION
                            )));
                            return Err(
                                acp::Error::internal_error().data("Protocol version too old")
                            );
                        }
                        debug!("ACP connection established, agent: {:?}", resp.agent_info);
                        let _ = init_tx.send(Ok((
                            connection.clone(),
                            resp.agent_capabilities,
                            resp.meta,
                        )));

                        futures::future::pending::<Result<(), acp::Error>>().await
                    }
                    Err(e) => {
                        let _ =
                            init_tx.send(Err(anyhow::anyhow!("ACP initialization failed: {e}")));
                        Err(e)
                    }
                }
            })
            .await;

        if let Err(e) = result {
            debug!("ACP connection task ended: {e}");
        }
    });

    // The agent's output ended. A process that exited says so itself, through its exit watcher.
    // One that is still alive without its standard output can never answer again: it is stopped,
    // and the session is told, instead of waiting on it forever.
    if let Some(watch) = output_watch {
        tokio::spawn(async move {
            if watch.closed.await.is_err() {
                return;
            }
            let mut exit = watch.exit_rx;
            let exited = tokio::select! {
                () = async { let _ = exit.wait_for(std::option::Option::is_some).await; } => true,
                () = tokio::time::sleep(OUTPUT_CLOSED_GRACE) => false,
            };
            if !exited {
                warn!("ACP agent closed its standard output but is still running; stopping it");
                watch.kill.notify_one();
                let _ = event_tx_for_closed
                    .send(ConnectionEvent::ChildExited {
                        status: None,
                        stderr_tail: "the agent closed its standard output but kept running, so \
                                      Nori stopped it"
                            .to_owned(),
                    })
                    .await;
            }
        });
    }

    let (cx, capabilities, initialize_meta) = init_rx
        .await
        .context("ACP connection task died during initialization")??;

    Ok(AcpConnection {
        cx,
        agent_capabilities: capabilities,
        initialize_meta,
        session_meta: std::sync::RwLock::new(None),
        in_process: false,
        event_rx,
        event_tx: connection_event_tx,
        session_config_state,
        children,
        connection_task,
        child: None,
        stderr_task: None,
    })
}

impl AcpConnection {
    /// Spawn a new ACP agent subprocess and establish a connection.
    pub async fn spawn(
        config: &AcpAgentConfig,
        cwd: &Path,
        proxy_config: AcpProxyConfig,
    ) -> Result<Self> {
        Self::spawn_inner(config, cwd, proxy_config, None).await
    }

    /// Spawn an ACP agent while routing the initialize response to an external sink.
    ///
    /// The initialize response is sent exclusively to `initialize_event_tx`; it is
    /// not duplicated in the connection's ordered event receiver. This lets callers
    /// preserve a raw ACP initialization error even when spawning returns an error.
    pub async fn spawn_with_initialize_event_sender(
        config: &AcpAgentConfig,
        cwd: &Path,
        proxy_config: AcpProxyConfig,
        initialize_event_tx: mpsc::Sender<AcpEvent>,
    ) -> Result<Self> {
        Self::spawn_inner(config, cwd, proxy_config, Some(initialize_event_tx)).await
    }

    async fn spawn_inner(
        config: &AcpAgentConfig,
        cwd: &Path,
        proxy_config: AcpProxyConfig,
        initialize_event_tx: Option<mpsc::Sender<AcpEvent>>,
    ) -> Result<Self> {
        if let Some(spec) = &config.in_process {
            return Self::spawn_in_process(config, spec, cwd, initialize_event_tx).await;
        }
        debug!(
            "Spawning ACP agent: {} {:?} in {}",
            config.command,
            config.args,
            cwd.display()
        );

        // --- Spawn the agent subprocess ---
        let mut cmd = Command::new(&config.command);
        cmd.args(&config.args)
            .envs(&config.env)
            .env_remove("CODEX_HOME")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Process group isolation and parent death signal.
        #[cfg(unix)]
        unsafe {
            #[cfg(target_os = "linux")]
            let parent_pid = libc::getpid();

            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                #[cfg(target_os = "linux")]
                {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent_pid {
                        libc::raise(libc::SIGTERM);
                    }
                }

                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn ACP agent: {}", config.command))?;

        let stdout = child.stdout.take().context("Failed to take stdout")?;
        let stdin = child.stdin.take().context("Failed to take stdin")?;
        let stderr = child.stderr.take().context("Failed to take stderr")?;
        let pid = child.id().context("Failed to get ACP agent pid")?;

        debug!("ACP agent spawned (pid: Some({pid}))");

        let wire_logger = if proxy_config.enabled {
            Some(WireLogger::new(&proxy_config, config, pid)?)
        } else {
            None
        };

        // Log stderr in the background, keeping a bounded tail so child
        // failures can surface the real cause (e.g. an auth hint) to the user
        // instead of only to the log file.
        let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<
            String,
        >::new()));
        let stderr_task = tokio::spawn({
            let stderr_tail = std::sync::Arc::clone(&stderr_tail);
            async move {
                let mut stderr = BufReader::new(stderr.compat());
                let mut line = String::new();
                while let Ok(n) = stderr.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    let trimmed = line.trim();
                    warn!("ACP agent stderr: {trimmed}");
                    if let Ok(mut tail) = stderr_tail.lock() {
                        if tail.len() >= STDERR_TAIL_LINES {
                            tail.pop_front();
                        }
                        tail.push_back(trimmed.to_string());
                    }
                    line.clear();
                }
            }
        });

        // The exit watcher owns the `Child`: it observes exit, sweeps the
        // process group before reaping, publishes the exit status, and reports
        // unexpected deaths into the ordered event stream. Without it, a dead
        // child is a silent EOF that the ACP layer treats as non-terminal —
        // pending requests hang forever.
        let (event_tx, event_rx) = mpsc::channel::<ConnectionEvent>(1024);
        let (exit_tx, exit_rx) = tokio::sync::watch::channel(None);
        let kill_notify = std::sync::Arc::new(tokio::sync::Notify::new());
        let group_cleanup_complete = std::sync::Arc::new(std::sync::Mutex::new(false));
        tokio::spawn({
            let event_tx = event_tx.clone();
            let stderr_tail = std::sync::Arc::clone(&stderr_tail);
            let kill_notify = std::sync::Arc::clone(&kill_notify);
            let group_cleanup_complete = std::sync::Arc::clone(&group_cleanup_complete);
            async move {
                let status =
                    wait_for_child_exit(child, pid, kill_notify, group_cleanup_complete).await;
                debug!("ACP agent exited: {status:?}");
                let _ = exit_tx.send(status);
                // Give the stderr reader a moment to drain the pipe before
                // snapshotting the tail.
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                let _ = event_tx
                    .send(ConnectionEvent::ChildExited {
                        status: status.and_then(|s| s.code()),
                        stderr_tail: collect_stderr_tail(&stderr_tail),
                    })
                    .await;
            }
        });
        // Install ownership immediately after the child moves into the exit
        // watcher. Until this handle is transferred into `AcpConnection`, its
        // Drop implementation makes cancellation of the initialization future
        // kill and reap the already-spawned process.
        let child_handle = ChildHandle {
            pid,
            exit_rx,
            kill: kill_notify,
            group_cleanup_complete,
        };

        let outgoing_logger = wire_logger.clone();
        let outgoing_sink = futures::sink::unfold(
            Box::pin(stdin.compat_write()),
            move |mut writer, line: String| {
                let logger = outgoing_logger.clone();
                async move {
                    if let Some(logger) = &logger {
                        logger.record(WireDirection::ClientToAgent, &line);
                    }
                    let mut bytes = line.into_bytes();
                    bytes.push(b'\n');
                    writer.write_all(&bytes).await?;
                    Ok::<_, std::io::Error>(writer)
                }
            },
        );

        let incoming_logger = wire_logger;
        let (output_closed_tx, output_closed_rx) = oneshot::channel::<()>();
        let incoming_lines = Box::pin(
            BufReader::new(stdout.compat())
                .lines()
                .map(move |line_result| {
                    if let Ok(line) = &line_result
                        && let Some(logger) = &incoming_logger
                    {
                        logger.record(WireDirection::AgentToClient, line);
                    }
                    line_result
                })
                // After the last line, say so: the protocol layer does not surface an end of
                // output while its connection is otherwise idle.
                .chain(
                    futures::stream::once(async move {
                        let _ = output_closed_tx.send(());
                    })
                    .filter_map(|()| async { None }),
                ),
        );
        let transport = Lines::new(outgoing_sink, incoming_lines);

        // Race initialization against the child dying: an agent that exits
        // immediately (e.g. unauthenticated) must fail spawn fast, with its
        // stderr as the cause — not hang the handshake forever.
        let mut exit_watch = child_handle.exit_rx.clone();
        // The watch Ref returned by wait_for is !Send; discard it inside the
        // branch future so the whole select stays Send.
        let child_died = async move {
            let _ = exit_watch.wait_for(std::option::Option::is_some).await;
        };
        let init = establish_connection(
            transport,
            cwd,
            event_tx,
            event_rx,
            initialize_event_tx,
            ConnectionOptions {
                advertise_subagents: config.advertise_subagents,
                agent_slug: config.provider_slug.clone(),
                children: SharedChildren::default(),
                output_watch: Some(OutputWatch {
                    exit_rx: child_handle.exit_rx.clone(),
                    kill: std::sync::Arc::clone(&child_handle.kill),
                    closed: output_closed_rx,
                }),
            },
        );
        let init_result = tokio::select! {
            result = init => result,
            () = child_died => Err(anyhow::anyhow!("agent exited before initialization finished")),
        };
        let mut connection = match init_result {
            Ok(connection) => connection,
            Err(error) => {
                // Whichever branch lost the race, a dead child is the real
                // cause (a handshake error like a broken pipe is just the
                // symptom). Give the watcher and stderr reader a moment to
                // observe the exit, then prefer the child's status + stderr.
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                let exited = child_handle
                    .exit_rx
                    .borrow()
                    .as_ref()
                    .map(std::process::ExitStatus::code);
                let Some(status) = exited else {
                    return Err(error);
                };
                let tail = collect_stderr_tail(&stderr_tail);
                stderr_task.abort();
                let status_desc = match status {
                    Some(code) => format!("exit code {code}"),
                    None => "killed by signal".to_string(),
                };
                let cause = if tail.is_empty() {
                    String::new()
                } else {
                    format!(": {tail}")
                };
                anyhow::bail!(
                    "ACP agent {} exited during startup ({status_desc}){cause}",
                    config.command
                );
            }
        };
        connection.child = Some(child_handle);
        connection.stderr_task = Some(stderr_task);
        Ok(connection)
    }

    /// Run a built-in agent inside this process, with its ACP client on the other end of an
    /// in-memory pipe. No process is started, so there is nothing to spawn, wait for, or reap:
    /// the handshake is the only start-up cost.
    async fn spawn_in_process(
        config: &AcpAgentConfig,
        spec: &crate::in_process::InProcessSpec,
        cwd: &Path,
        initialize_event_tx: Option<mpsc::Sender<AcpEvent>>,
    ) -> Result<Self> {
        let launcher = crate::in_process::in_process_launcher(&spec.agent).with_context(|| {
            format!(
                "this build of Nori has no built-in {} agent; rebuild with it enabled",
                spec.agent
            )
        })?;
        let spec = crate::in_process::with_environment(spec, &config.env);
        // The account's home as the agent will see it: the provider's variable and its value (a
        // path), never the rest of the environment.
        let home_variable = nori_config::PROVIDERS
            .iter()
            .find(|provider| provider.native_agent == spec.agent)
            .and_then(|provider| provider.home)
            .and_then(|variable| {
                spec.environment
                    .get(variable.name)
                    .map(|value| format!("{}={value}", variable.name))
            });
        debug!(
            "Starting built-in agent {} for {} in {} (home {:?}, {})",
            spec.agent,
            config.provider_slug,
            cwd.display(),
            spec.home,
            home_variable.as_deref().unwrap_or("no home variable")
        );

        let (client_end, agent_end) = tokio::io::duplex(IN_PROCESS_PIPE_BYTES);
        // The agent says why it stopped, whether it failed or just ended: a closed pipe is a
        // silent EOF to the protocol layer, which would leave the handshake waiting forever.
        let (stopped_tx, mut stopped_rx) = oneshot::channel::<String>();
        let agent_name = spec.agent.clone();
        let agent_task = tokio::spawn(async move {
            let reason = match launcher(spec, agent_end, crate::in_process::Host::Nori).await {
                Ok(()) => format!("built-in agent {agent_name} ended before it answered"),
                Err(error) => {
                    warn!("built-in agent {agent_name} stopped: {error}");
                    error.to_string()
                }
            };
            let _ = stopped_tx.send(reason);
        });

        let (read, write) = tokio::io::split(client_end);
        let outgoing_sink = futures::sink::unfold(
            Box::pin(write.compat_write()),
            move |mut writer, line: String| async move {
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                writer.write_all(&bytes).await?;
                Ok::<_, std::io::Error>(writer)
            },
        );
        let incoming_lines = Box::pin(BufReader::new(read.compat()).lines());
        let transport = Lines::new(outgoing_sink, incoming_lines);

        let (event_tx, event_rx) = mpsc::channel::<ConnectionEvent>(1024);
        let init = tokio::select! {
            result = establish_connection(
                transport,
                cwd,
                event_tx,
                event_rx,
                initialize_event_tx,
                ConnectionOptions {
                    advertise_subagents: config.advertise_subagents,
                    agent_slug: config.provider_slug.clone(),
                    children: SharedChildren::default(),
                    output_watch: None,
                },
            ) => result,
            Ok(reason) = &mut stopped_rx => Err(anyhow::anyhow!(reason)),
        };
        match init {
            Ok(mut connection) => {
                // The agent task takes the place of the subprocess handles: it is stopped when
                // the connection is.
                connection.stderr_task = Some(agent_task);
                connection.in_process = true;
                // Watch the agent for the connection's whole life, as the exit watcher does a
                // subprocess: an agent that ends or panics mid-turn is a dead connection, and the
                // ordered inbox says so. Without this its death is a silent EOF and the turn it
                // was in never ends. An intentional shutdown aborts the task, which the
                // closed channel and the session's shutdown flag make harmless.
                let event_tx = connection.event_tx.clone();
                let agent_name = config.command.clone();
                tokio::spawn(async move {
                    let reason = stopped_rx
                        .await
                        .unwrap_or_else(|_| format!("built-in agent {agent_name} stopped"));
                    let _ = event_tx
                        .send(ConnectionEvent::ChildExited {
                            status: None,
                            stderr_tail: reason,
                        })
                        .await;
                });
                Ok(connection)
            }
            Err(error) => {
                agent_task.abort();
                Err(error)
            }
        }
    }

    fn set_root_session(&self, session_id: acp::SessionId) {
        self.children
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .set_root(session_id);
    }

    /// Create a new session with the agent.
    ///
    /// `mcp_servers` are forwarded to the agent so it can connect to CLI-configured
    /// MCP servers. Pass an empty vec for sessions that don't need MCP (e.g. hooks).
    pub async fn create_session(
        &self,
        cwd: &Path,
        mcp_servers: Vec<acp::McpServer>,
    ) -> Result<acp::SessionId> {
        let request = self.cx.send_request(
            acp::NewSessionRequest::new(cwd)
                .mcp_servers(mcp_servers)
                .meta(self.session_meta()),
        );
        let request_id = schema_request_id(request.id());
        let response = request.block_task().await;
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: response.clone().map(acp::AgentResponse::NewSessionResponse),
            })))
            .await;
        let response = response.context("Failed to create ACP session")?;
        self.set_root_session(response.session_id.clone());

        if let Some(config_options) = response.config_options
            && let Ok(mut state) = self.session_config_state.write()
        {
            state.config_options = config_options;
        }

        Ok(response.session_id)
    }

    /// Load (resume) an existing session.
    ///
    /// The agent replays previous session history. Updates flow through the
    /// ordered event inbox. The returned `SessionId` is the same as
    /// the input `session_id` (the LoadSessionResponse doesn't contain one).
    pub async fn load_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<acp::McpServer>,
        request_started: Option<oneshot::Sender<RequestId>>,
    ) -> Result<acp::SessionId> {
        // Replay updates can arrive before the response, and are the root's.
        self.set_root_session(acp::SessionId::new(session_id));
        let request = self.cx.send_request(
            acp::LoadSessionRequest::new(session_id.to_string(), cwd)
                .mcp_servers(mcp_servers)
                .meta(self.session_meta()),
        );
        let request_id = schema_request_id(request.id());
        if let Some(request_started) = request_started {
            let _ = request_started.send(request_id.clone());
        }
        let response = request.block_task().await;
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: response
                    .clone()
                    .map(acp::AgentResponse::LoadSessionResponse),
            })))
            .await;
        let response = response.context("Failed to load ACP session")?;

        if let Some(config_options) = response.config_options
            && let Ok(mut state) = self.session_config_state.write()
        {
            state.config_options = config_options;
        }

        // The session ID from the request is reused since the response
        // doesn't contain one.
        Ok(acp::SessionId::from(session_id.to_string()))
    }

    /// Resume (reattach to) an existing session via ACP `session/resume`.
    ///
    /// Unlike `session/load`, resume is a live reattach with no history
    /// replay. Capability combinations describe the ACP facade, not whether
    /// its session runs locally or in the cloud.
    /// The returned `SessionId` is the input id (the response carries none).
    pub async fn resume_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<acp::McpServer>,
    ) -> Result<acp::SessionId> {
        self.resume_session_with_request_started(session_id, cwd, mcp_servers, None)
            .await
    }

    /// Resume a session, optionally reporting the ACP request ID before awaiting its response.
    pub async fn resume_session_with_request_started(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<acp::McpServer>,
        request_started: Option<oneshot::Sender<RequestId>>,
    ) -> Result<acp::SessionId> {
        self.set_root_session(acp::SessionId::new(session_id));
        let request = self.cx.send_request(
            acp::ResumeSessionRequest::new(session_id.to_string(), cwd)
                .mcp_servers(mcp_servers)
                .meta(self.session_meta()),
        );
        let request_id = schema_request_id(request.id());
        if let Some(request_started) = request_started {
            let _ = request_started.send(request_id.clone());
        }
        let response = request.block_task().await;
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: response
                    .clone()
                    .map(acp::AgentResponse::ResumeSessionResponse),
            })))
            .await;
        let response = response.context("Failed to resume ACP session")?;

        if let Some(config_options) = response.config_options
            && let Ok(mut state) = self.session_config_state.write()
        {
            state.config_options = config_options;
        }

        Ok(acp::SessionId::from(session_id.to_string()))
    }

    /// Fork an existing session at its current head via ACP `session/fork`.
    ///
    /// Unlike `session/load` and `session/resume`, fork returns a NEW session
    /// id (the forked session), so the returned `SessionId` comes from the
    /// response rather than echoing the input.
    pub async fn fork_session(
        &self,
        session_id: &str,
        cwd: &Path,
        mcp_servers: Vec<acp::McpServer>,
    ) -> Result<acp::SessionId> {
        let request = self.cx.send_request(
            acp::ForkSessionRequest::new(session_id.to_string(), cwd)
                .mcp_servers(mcp_servers)
                .meta(self.session_meta()),
        );
        let request_id = schema_request_id(request.id());
        let response = request.block_task().await;
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: response
                    .clone()
                    .map(acp::AgentResponse::ForkSessionResponse),
            })))
            .await;
        let response = response.context("Failed to fork ACP session")?;
        self.set_root_session(response.session_id.clone());

        if let Some(config_options) = response.config_options
            && let Ok(mut state) = self.session_config_state.write()
        {
            state.config_options = config_options;
        }

        Ok(response.session_id)
    }

    /// Close (release) a session via ACP `session/close`.
    pub async fn close_session(&self, session_id: &str) -> Result<()> {
        let request = self
            .cx
            .send_request(acp::CloseSessionRequest::new(session_id.to_string()));
        let request_id = schema_request_id(request.id());
        let response = request.block_task().await;
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: response
                    .clone()
                    .map(acp::AgentResponse::CloseSessionResponse),
            })))
            .await;
        response.context("Failed to close ACP session")?;
        let _ = self.event_tx.send(ConnectionEvent::SessionClosed).await;
        Ok(())
    }

    /// List the agent's known sessions via ACP `session/list`.
    ///
    /// `cwd` is forwarded as the spec's working-directory filter. Cursor-based
    /// pagination is drained internally, so the returned vector contains every
    /// page concatenated in agent order.
    pub async fn list_sessions(&self, cwd: &Path) -> Result<Vec<acp::SessionInfo>> {
        // Bound pagination so a misbehaving agent that keeps returning a
        // `next_cursor` cannot pin the picker in an unbounded loop.
        const MAX_SESSION_LIST_PAGES: usize = 1000;

        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;

        for _ in 0..MAX_SESSION_LIST_PAGES {
            let mut request = acp::ListSessionsRequest::new().cwd(cwd.to_path_buf());
            if let Some(cursor) = cursor.take() {
                request = request.cursor(cursor);
            }

            let request = self.cx.send_request(request);
            let request_id = schema_request_id(request.id());
            let response = request.block_task().await;
            let _ = self
                .event_tx
                .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                    request_id,
                    response: response
                        .clone()
                        .map(acp::AgentResponse::ListSessionsResponse),
                })))
                .await;
            let response = response.context("Failed to list ACP sessions")?;

            sessions.extend(response.sessions);

            match response.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        Ok(sessions)
    }

    /// Send a prompt to an existing session and receive streaming updates.
    ///
    /// Updates flow through the ordered event inbox.
    pub async fn prompt(
        &self,
        session_id: acp::SessionId,
        prompt: Vec<acp::ContentBlock>,
    ) -> Result<acp::StopReason> {
        let (_, result) = self
            .prompt_attempt(session_id, prompt, None, None, None, true)
            .await;
        result
    }

    /// Send a prompt and report its transport-assigned request ID as soon as the request is
    /// issued.
    ///
    /// A caller that may try again passes `mirror_failure: false`, so a failed attempt is not
    /// published as the turn's response, and `report_as` (the first attempt's ID) so that the
    /// response of a later attempt is published under the one ID the turn has. Such a caller
    /// publishes a failure it gives up on with [`Self::announce_prompt_failure`].
    pub async fn prompt_attempt(
        &self,
        session_id: acp::SessionId,
        prompt: Vec<acp::ContentBlock>,
        prompt_meta: Option<acp::Meta>,
        request_started: Option<oneshot::Sender<Result<RequestId>>>,
        report_as: Option<RequestId>,
        mirror_failure: bool,
    ) -> (RequestId, Result<acp::StopReason>) {
        debug!(
            target: "acp_event_flow",
            session_id = %session_id,
            content_blocks = prompt.len(),
            "Transport sending ACP session/prompt request"
        );
        let request = self
            .cx
            .send_request(acp::PromptRequest::new(session_id.clone(), prompt).meta(prompt_meta));
        let request_id = schema_request_id(request.id());
        if let Some(request_started) = request_started {
            let _ = request_started.send(Ok(request_id.clone()));
        }
        let response = request.block_task().await;
        if response.is_ok() || mirror_failure {
            let _ = self
                .event_tx
                .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                    request_id: report_as.unwrap_or_else(|| request_id.clone()),
                    response: response.clone().map(acp::AgentResponse::PromptResponse),
                })))
                .await;
        }
        let result = response.context("ACP prompt failed").map(|response| {
            debug!(
                target: "acp_event_flow",
                session_id = %session_id,
                stop_reason = ?response.stop_reason,
                "Transport received ACP session/prompt response"
            );
            response.stop_reason
        });
        (request_id, result)
    }

    /// Publish the failure of a prompt whose attempts were not mirrored as its response, once
    /// the caller has given up on it. Does nothing when `error` carries no ACP error.
    pub async fn announce_prompt_failure(&self, request_id: RequestId, error: &anyhow::Error) {
        let Some(error) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<acp::Error>())
        else {
            return;
        };
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: Err(error.clone()),
            })))
            .await;
    }

    /// Publish the end of a prompt that no attempt completed, such as a retry the user cancelled.
    pub async fn announce_prompt_stop(&self, request_id: RequestId, stop_reason: acp::StopReason) {
        let _ = self
            .event_tx
            .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                request_id,
                response: Ok(acp::AgentResponse::PromptResponse(
                    acp::PromptResponse::new(stop_reason),
                )),
            })))
            .await;
    }

    /// Wait until the harness has handled preceding connection events.
    pub async fn event_barrier(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.event_tx
            .send(ConnectionEvent::Barrier(tx))
            .await
            .context("ACP event relay closed")?;
        rx.await.context("ACP event relay closed")
    }

    /// Cancel an ongoing prompt.
    pub async fn cancel(&self, session_id: &acp::SessionId) -> Result<()> {
        self.cx
            .send_notification(acp::CancelNotification::new(session_id.clone()))
            .context("Failed to cancel ACP session")
    }

    /// Get the agent's capabilities.
    pub fn capabilities(&self) -> &acp::AgentCapabilities {
        &self.agent_capabilities
    }

    /// Get the top-level `_meta` of the initialize response, where agents
    /// advertise ACP extensions.
    pub fn initialize_meta(&self) -> Option<&acp::Meta> {
        self.initialize_meta.as_ref()
    }

    /// Send `meta` as the `_meta` of every later session request, or none.
    pub fn set_session_meta(&self, meta: Option<acp::Meta>) {
        *self
            .session_meta
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = meta;
    }

    fn session_meta(&self) -> Option<acp::Meta> {
        self.session_meta
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Whether the agent runs inside this process, where host tools can be handed to it.
    pub fn is_in_process(&self) -> bool {
        self.in_process
    }

    /// Send an extension (`_`-prefixed) method request to the agent and
    /// return its raw JSON response.
    pub async fn send_ext_request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let request = agent_client_protocol::UntypedMessage::new(method, params)
            .with_context(|| format!("failed to encode {method} request"))?;
        self.cx
            .send_request(request)
            .block_task()
            .await
            .with_context(|| format!("{method} request failed"))
    }

    /// Take ownership of the ordered ACP event receiver.
    pub fn take_event_receiver(&mut self) -> mpsc::Receiver<ConnectionEvent> {
        std::mem::replace(&mut self.event_rx, mpsc::channel(1).1)
    }

    /// Get the current ACP session config snapshot.
    pub fn config_options(&self) -> Vec<acp::SessionConfigOption> {
        #[expect(
            clippy::expect_used,
            reason = "RwLock poisoning indicates a bug elsewhere"
        )]
        self.session_config_state
            .read()
            .expect("Session config state lock poisoned")
            .config_options
            .clone()
    }

    /// Set an ACP session config option and replace state from the full response snapshot.
    pub async fn set_config_option(
        &self,
        session_id: &acp::SessionId,
        config_id: impl Into<acp::SessionConfigId>,
        value: impl Into<acp::SessionConfigValueId>,
    ) -> Result<()> {
        let value = acp::SessionConfigOptionValue::value_id(value.into());
        let request = self
            .cx
            .send_request(acp::SetSessionConfigOptionRequest::new(
                session_id.clone(),
                config_id,
                value,
            ));
        let request_id = schema_request_id(request.id());
        let response = request.block_task().await;
        // Only mirror a successful response onto the public boundary. Unlike
        // prompt/new-session requests (whose raw errors are the user-facing
        // surface), a config-option request's outcome is returned to the caller
        // and rendered by the `/model` flow — which shows a friendly message and
        // may persist-and-restart. Mirroring the raw JSON-RPC error here too
        // would surface a confusing duplicate "Internal error" cell.
        if let Ok(ok_response) = &response {
            let _ = self
                .event_tx
                .send(ConnectionEvent::Acp(Box::new(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::AgentResponse::SetSessionConfigOptionResponse(
                        ok_response.clone(),
                    )),
                })))
                .await;
        }
        // The agent says why it refused (a rate limit, a model the account lacks) in the error's
        // data; that is what the user needs to read, not the name of the request.
        let response = response.map_err(|error| {
            let detail = error.data.as_ref().and_then(|data| {
                data.get("details")
                    .or_else(|| data.get("detail"))
                    .unwrap_or(data)
                    .as_str()
                    .map(str::to_owned)
            });
            match detail {
                Some(detail) => anyhow::anyhow!("{detail}"),
                None => {
                    anyhow::Error::new(error).context("Failed to set ACP session config option")
                }
            }
        })?;

        if let Ok(mut state) = self.session_config_state.write() {
            state.config_options = response.config_options;
        }

        Ok(())
    }

    /// Set a child session's config without changing the root session snapshot.
    pub async fn set_child_config_option(
        &self,
        session_id: &acp::SessionId,
        config_id: acp::SessionConfigId,
        value: acp::SessionConfigValueId,
    ) -> Result<Vec<acp::SessionConfigOption>> {
        let request = self
            .cx
            .send_request(acp::SetSessionConfigOptionRequest::new(
                session_id.clone(),
                config_id,
                acp::SessionConfigOptionValue::value_id(value),
            ));
        let response = request.block_task().await.map_err(|error| {
            let detail = error.data.as_ref().and_then(|data| {
                data.get("details")
                    .or_else(|| data.get("detail"))
                    .unwrap_or(data)
                    .as_str()
                    .map(str::to_owned)
            });
            match detail {
                Some(detail) => anyhow::anyhow!("{detail}"),
                None => {
                    anyhow::Error::new(error).context("Failed to set child session config option")
                }
            }
        })?;
        Ok(response.config_options)
    }

    /// Explicitly tear down the ACP subprocess and background tasks with the
    /// default grace period.
    ///
    /// Unlike `Drop`, this async path can wait for process termination so the
    /// child is reaped promptly during agent switches and shutdown.
    pub async fn shutdown(&self) {
        self.shutdown_with_grace(SHUTDOWN_GRACE).await;
    }

    /// Tear down the ACP subprocess gracefully: close its stdin, give it
    /// `grace` to exit on its own (agents like `nori-handroll cloud-acp` run
    /// their release path on stdin EOF), then kill whatever is left.
    pub async fn shutdown_with_grace(&self, grace: std::time::Duration) {
        // Aborting the connection task drops the transport and with it the
        // child's stdin writer — the EOF is the agent's shutdown signal.
        self.connection_task.abort();

        if let Some(ref child) = self.child {
            let mut exit_rx = child.exit_rx.clone();
            let exited_in_time =
                tokio::time::timeout(grace, exit_rx.wait_for(std::option::Option::is_some))
                    .await
                    .is_ok();
            if !exited_in_time {
                debug!("ACP agent did not exit within {grace:?} of stdin EOF; killing");
                child.request_kill();
                // Wait for the watcher to reap so no zombie outlives shutdown.
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    exit_rx.wait_for(std::option::Option::is_some),
                )
                .await;
            }
        }

        if let Some(ref stderr_task) = self.stderr_task {
            stderr_task.abort();
        }
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        self.connection_task.abort();
        if let Some(ref stderr_task) = self.stderr_task {
            stderr_task.abort();
        }

        // Backstop for paths that never ran `shutdown()`: kill outright.
        if let Some(ref child) = self.child {
            child.request_kill();
        }
    }
}
