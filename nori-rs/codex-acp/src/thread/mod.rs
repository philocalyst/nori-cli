//! One Codex thread, seen as an ACP session.
//!
//! [`Thread`] is the handle the agent holds; the work happens on a [`actor::ThreadActor`]
//! task that owns the Codex thread. The modules split the translation by what Codex is doing:
//!
//! - `actor`, `submit`, `config_options`, `modes`, `replay`: the session's own operations.
//! - `prompt`: per-submission state and the dispatch of Codex events.
//! - `exec`, `patches`, `tool_calls`, `media`, `guardian`: the event families.
//! - `permissions`, `mcp_approval`: questions to the client and their answers.
//! - `usage`, `errors`: token and rate-limit reports, and failures as the host reads them.
//! - `client`, `backends`: the seams tests replace.
//! - `child_stream`: Codex subagent threads, forwarded on their own session ids.

mod actor;
mod backends;
mod child_stream;
mod client;
mod config_options;
mod errors;
mod exec;
mod guardian;
mod mcp_approval;
mod media;
mod modes;
mod patches;
mod permissions;
mod prompt;
mod replay;
mod submit;
#[cfg(test)]
mod tests;
mod tool_calls;
mod usage;

pub(crate) use backends::CodexThreadImpl;
pub(crate) use backends::LiveModels;
pub(crate) use child_stream::ChildPrompt;
pub(crate) use child_stream::ChildPrompts;

use std::sync::Arc;
use std::sync::Mutex;

use crate::thread::actor::ThreadActor;
use crate::thread::backends::ModelsManagerImpl;
use crate::thread::client::SessionClient;
use codex_core::config::Config;
use codex_history::RolloutItem;
use codex_login::auth::AuthManager;
use codex_protocol::protocol::Op;
use nori_acp_kit::Cx;
use nori_acp_kit::TurnGate;
use nori_acp_kit::TurnPolicy;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::ClientCapabilities;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::RequestPermissionResponse;
use nori_acp_kit::schema::SessionConfigId;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionConfigOptionValue;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::SessionModeId;
use nori_acp_kit::schema::StopReason;
use nori_host_tools::HostTools;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

enum ThreadMessage {
    Load {
        response_tx: oneshot::Sender<Result<LoadSessionResponse, Error>>,
    },
    GetConfigOptions {
        response_tx: oneshot::Sender<Result<Vec<SessionConfigOption>, Error>>,
    },
    Prompt {
        request: PromptRequest,
        response_tx: oneshot::Sender<Result<oneshot::Receiver<Result<StopReason, Error>>, Error>>,
    },
    SetMode {
        mode: SessionModeId,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    SetConfigOption {
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    Cancel {
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    Shutdown {
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    ReplayHistory {
        history: Vec<RolloutItem>,
        response_tx: oneshot::Sender<Result<(), Error>>,
    },
    PermissionRequestResolved {
        submission_id: String,
        interaction_id: u64,
        request_key: String,
        response: Result<RequestPermissionResponse, Error>,
    },
}

pub struct Thread {
    /// Direct handle to the underlying Codex thread for out-of-band shutdown.
    thread: Arc<dyn CodexThreadImpl>,
    /// Codex folds a prompt that arrives mid-turn into the running turn (start-or-steer), so the
    /// gate admits every prompt and cancels them together.
    gate: TurnGate,
    /// A sender for interacting with the thread.
    message_tx: mpsc::UnboundedSender<ThreadMessage>,
    /// Keep the actor task alive for the lifetime of the thread wrapper.
    _handle: tokio::task::JoinHandle<()>,
}

impl Thread {
    #[expect(clippy::too_many_arguments)]
    pub fn new(
        session_id: SessionId,
        thread: Arc<dyn CodexThreadImpl>,
        auth: Arc<AuthManager>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        client_capabilities: Arc<Mutex<ClientCapabilities>>,
        config: Config,
        host_tools: Option<Arc<dyn HostTools>>,
        cx: Cx,
        thread_manager: Arc<codex_core::ThreadManager>,
        child_prompts: ChildPrompts,
    ) -> Self {
        let (message_tx, message_rx) = mpsc::unbounded_channel();
        let (resolution_tx, resolution_rx) = mpsc::unbounded_channel();

        let mut actor = ThreadActor::new(
            auth,
            SessionClient::new(session_id, cx, client_capabilities),
            thread.clone(),
            models_manager,
            config,
            message_rx,
            resolution_tx,
            resolution_rx,
        );
        actor.host_tools = host_tools;
        actor.child_streams = Some(child_stream::ChildStreams::new(
            thread_manager,
            child_prompts,
        ));
        let handle = tokio::spawn(actor.spawn());

        Self {
            thread,
            gate: TurnGate::new(TurnPolicy::Steerable),
            message_tx,
            _handle: handle,
        }
    }

    pub async fn load(&self) -> Result<LoadSessionResponse, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Load { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn config_options(&self) -> Result<Vec<SessionConfigOption>, Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::GetConfigOptions { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn prompt(&self, request: PromptRequest) -> Result<StopReason, Error> {
        // Held until the response is produced: the session is running while any prompt waits.
        let turn = self.gate.begin().await?;
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Prompt {
            request,
            response_tx,
        };
        drop(self.message_tx.send(message));

        let stop = response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))??
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))??;
        // A cancelled prompt answers `Cancelled`, whichever way Codex ended the turn.
        Ok(if turn.is_cancelled() {
            StopReason::Cancelled
        } else {
            stop
        })
    }

    pub async fn set_mode(&self, mode: SessionModeId) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetMode { mode, response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn set_config_option(
        &self,
        config_id: SessionConfigId,
        value: SessionConfigOptionValue,
    ) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::SetConfigOption {
            config_id,
            value,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn cancel(&self) -> Result<(), Error> {
        self.gate.cancel();
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::Cancel { response_tx };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn replay_history(&self, history: Vec<RolloutItem>) -> Result<(), Error> {
        let (response_tx, response_rx) = oneshot::channel();

        let message = ThreadMessage::ReplayHistory {
            history,
            response_tx,
        };
        drop(self.message_tx.send(message));

        response_rx
            .await
            .map_err(|e| Error::internal_error().data(e.to_string()))?
    }

    pub async fn shutdown(&self) -> Result<(), Error> {
        self.gate.cancel();
        let (response_tx, response_rx) = oneshot::channel();
        let message = ThreadMessage::Shutdown { response_tx };

        if self.message_tx.send(message).is_err() {
            self.thread
                .submit(Op::Shutdown)
                .await
                .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        } else {
            response_rx
                .await
                .map_err(|e| Error::internal_error().data(e.to_string()))??;
        }
        // Let the actor drain the resulting turn-aborted/shutdown events so any in-flight
        // prompt callers observe a clean cancellation instead of a dropped response channel.
        Ok(())
    }
}
