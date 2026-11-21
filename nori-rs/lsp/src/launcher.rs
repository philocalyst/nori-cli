//! Starting language-server processes. The manager only knows the [`ServerLauncher`] trait, so a
//! host can launch servers somewhere else (a remote machine's shell) and tests can start an
//! in-process fake server over a pipe.

use std::path::PathBuf;
use std::sync::Arc;

use helix_lsp::Call;
use helix_lsp::Client;
use helix_lsp::LanguageServerId;
use helix_lsp::lsp;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::error::LspError;
use crate::registry::ServerConfig;

#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub server: ServerConfig,
    /// The resolved binary, as [`ServerLauncher::resolve`] returned it.
    pub command: PathBuf,
    pub root: PathBuf,
    pub id: LanguageServerId,
    pub request_timeout_seconds: u64,
}

/// A client whose handshake has not run yet, with the stream of messages the server sends.
pub struct Launched {
    pub client: Client,
    pub incoming: UnboundedReceiver<(LanguageServerId, Call)>,
    pub initialize_notify: Arc<Notify>,
}

pub trait ServerLauncher: Send + Sync + 'static {
    /// The binary for `server`, or `None` when it is not installed. May touch the filesystem, so
    /// the manager calls it from a blocking thread.
    fn resolve(&self, server: &ServerConfig) -> Option<PathBuf>;

    /// Starts the server. Called inside a tokio runtime.
    fn launch(&self, request: LaunchRequest) -> Result<Launched, LspError>;
}

/// Runs servers as child processes of this process, found on `PATH`.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessLauncher;

impl ServerLauncher for ProcessLauncher {
    fn resolve(&self, server: &ServerConfig) -> Option<PathBuf> {
        which::which(&server.command).ok()
    }

    fn launch(&self, request: LaunchRequest) -> Result<Launched, LspError> {
        let root_uri = lsp::Url::from_directory_path(&request.root).ok();
        let (client, incoming, initialize_notify) = Client::start(
            request.command.as_os_str(),
            &request.server.args,
            request.server.initialization_options.clone(),
            request.server.env.iter().map(|(key, value)| (key, value)),
            request.root.clone(),
            root_uri,
            request.id,
            request.server.name.clone(),
            request.request_timeout_seconds,
        )
        .map_err(|error| LspError::ServerUnavailable {
            server: request.server.name.clone(),
            reason: error.to_string(),
        })?;
        Ok(Launched {
            client,
            incoming,
            initialize_notify,
        })
    }
}
