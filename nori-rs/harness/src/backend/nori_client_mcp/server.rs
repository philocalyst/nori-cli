//! The loopback MCP server, the stdio socket for agents without HTTP MCP, and how a session
//! registers the `nori-client` tools with its agent.

use super::*;

/// The command, and the arguments before the socket path, that relays an agent's stdio to the
/// `nori-client` socket. Set once by the binary that can do it; without it, only agents that
/// speak HTTP MCP get the server.
static STDIO_BRIDGE: std::sync::OnceLock<(std::path::PathBuf, Vec<String>)> =
    std::sync::OnceLock::new();

/// Let agents without HTTP MCP reach the `nori-client` server by running `command args… <socket>`,
/// which must relay its stdin and stdout to the unix socket it is given.
pub fn set_stdio_bridge(command: std::path::PathBuf, args: Vec<String>) {
    let _ = STDIO_BRIDGE.set((command, args));
}

/// Whether an agent can be given the `nori-client` server.
pub(in crate::backend) fn reachable(connection: &AcpConnection) -> bool {
    connection.capabilities().mcp_capabilities.http || (cfg!(unix) && STDIO_BRIDGE.get().is_some())
}

/// Loopback `nori-client` MCP server. Bound on first use and torn down on drop.
pub(crate) struct NoriClientServer {
    pub(in crate::backend) local_tools: Arc<dyn nori_host_tools::HostTools>,
    url: String,
    pub(super) authorization_header_value: String,
    task: tokio::task::JoinHandle<()>,
    /// The stdio server for agents without HTTP MCP, when this agent needs it.
    #[cfg(unix)]
    socket: Option<SocketServer>,
}

/// The `nori-client` service on a unix socket in a directory only this user can enter.
#[cfg(unix)]
struct SocketServer {
    directory: std::path::PathBuf,
    path: std::path::PathBuf,
    task: tokio::task::JoinHandle<()>,
}

#[cfg(unix)]
impl SocketServer {
    fn spawn(shared: NoriClientShared) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;

        let directory = std::env::temp_dir().join(format!(
            "nori-mcp-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .context("failed to create the nori-client socket directory")?;
        let path = directory.join("mcp.sock");
        let listener = tokio::net::UnixListener::bind(&path)
            .context("failed to bind the nori-client socket")?;
        let task = tokio::spawn(async move {
            // Dropping the set when the server is torn down ends every connection.
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                let service = NoriClientService::new(shared.clone());
                connections.spawn(async move {
                    match rmcp::ServiceExt::serve(service, stream).await {
                        Ok(running) => {
                            let _ = running.waiting().await;
                        }
                        Err(err) => tracing::debug!("nori-client socket connection failed: {err}"),
                    }
                });
            }
        });
        Ok(Self {
            directory,
            path,
            task,
        })
    }
}

#[cfg(unix)]
impl Drop for SocketServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

impl NoriClientServer {
    /// The server, with a stdio entry point unless the agent speaks HTTP MCP.
    pub(crate) async fn spawn(shared: NoriClientShared, http: bool) -> Result<Self> {
        #[cfg(unix)]
        let socket = if http {
            None
        } else {
            Some(SocketServer::spawn(shared.clone())?)
        };
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .context("failed to bind local nori-client MCP server")?;
        let url = format!("http://{}/mcp", listener.local_addr()?);
        let authorization_header_value = format!("Bearer {}", uuid::Uuid::new_v4());
        let expected_authorization = Arc::new(authorization_header_value.clone());

        let local_tools = super::host_tools::from_shared(shared.clone());
        let service = StreamableHttpService::new(
            move || Ok(NoriClientService::new(shared.clone())),
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig {
                stateful_mode: false,
                sse_keep_alive: None,
                ..Default::default()
            },
        );
        let router = axum::Router::new().nest_service("/mcp", service).layer(
            middleware::from_fn_with_state(expected_authorization, require_bearer_token),
        );
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = ready_tx.send(());
            if let Err(err) = axum::serve(listener, router).await {
                tracing::debug!("nori-client MCP server stopped: {err}");
            }
        });
        ready_rx
            .await
            .context("nori-client MCP server task exited before becoming ready")?;

        Ok(Self {
            local_tools,
            url,
            authorization_header_value,
            task,
            #[cfg(unix)]
            socket,
        })
    }

    #[cfg(test)]
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    fn authorization_header(&self) -> acp::HttpHeader {
        acp::HttpHeader::new("Authorization", self.authorization_header_value.clone())
    }

    pub(in crate::backend) fn as_mcp_server(&self) -> acp::McpServer {
        #[cfg(unix)]
        if let (Some(socket), Some((command, args))) = (&self.socket, STDIO_BRIDGE.get()) {
            let mut args = args.clone();
            args.push(socket.path.display().to_string());
            return acp::McpServer::Stdio(
                acp::McpServerStdio::new("nori-client", command.clone()).args(args),
            );
        }
        acp::McpServer::Http(
            acp::McpServerHttp::new("nori-client", self.url.clone())
                .headers(vec![self.authorization_header()]),
        )
    }
}

impl Drop for NoriClientServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// How the session's agent reaches the `nori-client` tools.
pub(crate) enum NoriClient {
    /// A loopback MCP server, for an agent in another process.
    Mcp(NoriClientServer),
    /// Host tools handed straight to an agent built into this process. The connection sends them
    /// with every session request, so they need no entry in the session's MCP servers; holding
    /// the registration keeps them findable.
    Host {
        _registration: nori_host_tools::Registration,
    },
}

impl NoriClient {
    /// Continuations require both goal inspection and completion tools.
    pub(in crate::backend) fn has_goal_tools(&self) -> bool {
        let tools = match self {
            Self::Mcp(server) => Some(Arc::clone(&server.local_tools)),
            Self::Host { _registration } => {
                nori_host_tools::from_session_meta(Some(&_registration.session_meta()))
            }
        };
        tools.is_some_and(|tools| {
            let specs = tools.specs();
            ["get_goal", "update_goal"]
                .iter()
                .all(|name| specs.iter().any(|spec| spec.name == *name))
        })
    }
}

pub(in crate::backend) struct PendingNoriClientServer {
    server: Option<NoriClient>,
    connected: Arc<AtomicBool>,
    previous_connected: bool,
    committed: bool,
}

impl PendingNoriClientServer {
    /// Whether the tools go to the agent as host tools rather than over MCP.
    pub(in crate::backend) fn is_host(&self) -> bool {
        matches!(self.server, Some(NoriClient::Host { .. }))
    }

    pub(in crate::backend) async fn commit(mut self, http_server: &Arc<Mutex<Option<NoriClient>>>) {
        *http_server.lock().await = self.server.take();
        self.committed = true;
    }
}

impl Drop for PendingNoriClientServer {
    fn drop(&mut self) {
        if !self.committed {
            self.connected
                .store(self.previous_connected, Ordering::Relaxed);
        }
    }
}

async fn require_bearer_token(
    State(expected): State<Arc<String>>,
    request: Request<Body>,
    next: Next,
) -> std::result::Result<Response, StatusCode> {
    if request
        .headers()
        .get(AUTHORIZATION)
        .is_some_and(|value| value.as_bytes() == expected.as_bytes())
    {
        Ok(next.run(request).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

pub(in crate::backend) async fn register_for_session(
    connection: &AcpConnection,
    mcp_servers: &mut Vec<acp::McpServer>,
    thread_goal_state: Arc<Mutex<thread_goal::ThreadGoalState>>,
    backend_event_tx: mpsc::Sender<BackendEvent>,
    connected: Arc<AtomicBool>,
    orchestrator: super::orchestrator::Orchestrator,
    workspace: Workspace,
) -> Result<Option<PendingNoriClientServer>> {
    let host_tools = super::host_tools::accepted_by(connection);
    if !host_tools && !reachable(connection) {
        return Ok(None);
    }

    let selection = workspace
        .workflow
        .lock()
        .await
        .active
        .as_ref()
        .and_then(|workflow| workflow.tools.as_ref())
        .map(|tools| nori_host_tools::ToolSelection {
            enabled: tools
                .enabled
                .as_ref()
                .map(|names| names.iter().cloned().collect()),
            disabled: tools.disabled.iter().cloned().collect(),
            descriptions: tools.descriptions.clone(),
            instructions: tools.instructions.clone(),
        });
    // What the program that started this actor restricts it to narrows what its policy allows.
    let selection = match orchestrator.seed().scope.as_deref() {
        Some(restriction) => {
            let mut selection = selection.unwrap_or_default();
            if let Some(names) = restriction.tool_names() {
                selection.enabled = Some(match selection.enabled.take() {
                    Some(enabled) => enabled.intersection(&names).cloned().collect(),
                    None => names,
                });
            }
            selection
                .instructions
                .extend(restriction.instructions.iter().cloned());
            Some(selection)
        }
        None => selection,
    };
    let shared = NoriClientShared::new(
        thread_goal_state,
        backend_event_tx,
        Arc::clone(&connected),
        agent_capabilities_view(connection),
        true,
        goal_ext::GoalExtCapability::from_initialize_meta(connection.initialize_meta()).is_some(),
        orchestrator,
    )
    .with_workspace(workspace);
    let mut shared = shared;
    shared.tool_selection = selection;
    let previous_connected = connected.load(Ordering::Relaxed);
    let next_server = if host_tools {
        NoriClient::Host {
            _registration: super::host_tools::hand_to(connection, shared, &connected),
        }
    } else {
        let server =
            NoriClientServer::spawn(shared, connection.capabilities().mcp_capabilities.http)
                .await?;
        connected.store(false, Ordering::Relaxed);
        mcp_servers.push(server.as_mcp_server());
        NoriClient::Mcp(server)
    };

    Ok(Some(PendingNoriClientServer {
        server: Some(next_server),
        connected,
        previous_connected,
        committed: false,
    }))
}
