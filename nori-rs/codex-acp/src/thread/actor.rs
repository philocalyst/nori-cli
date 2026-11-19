//! The actor that owns one Codex thread: it serializes client messages, Codex events and
//! resolved permission questions onto one task.

use crate::thread::child_stream::ChildStreams;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::thread::ThreadMessage;
use crate::thread::backends::Auth;
use crate::thread::backends::CodexThreadImpl;
use crate::thread::backends::ModelsManagerImpl;
use crate::thread::client::SessionClient;
use crate::thread::prompt::SubmissionState;
use codex_core::config::Config;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::AvailableCommandsUpdate;
use nori_acp_kit::schema::LoadSessionResponse;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionUpdate;
use nori_host_tools::HostTools;
use tokio::sync::mpsc;
use tracing::error;
use tracing::warn;

pub(super) struct ThreadActor<A> {
    /// Allows for logging out from slash commands
    pub(super) auth: A,
    /// Used for sending messages back to the client.
    pub(super) client: SessionClient,
    /// The thread associated with this task.
    pub(super) thread: Arc<dyn CodexThreadImpl>,
    /// The configuration for the thread.
    pub(super) config: Config,
    /// The models available for this thread.
    pub(super) models_manager: Arc<dyn ModelsManagerImpl>,
    /// Internal message sender used to route spawned interaction results back to the actor.
    pub(super) resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
    /// A sender for each interested `Op` submission that needs events routed.
    pub(super) submissions: HashMap<String, SubmissionState>,
    /// A receiver for incoming thread messages.
    pub(super) message_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    /// A receiver for spawned interaction results.
    pub(super) resolution_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    /// Last config options state we emitted to the client, used for deduping updates.
    pub(super) last_sent_config_options: Option<Vec<SessionConfigOption>>,
    /// The host's tools, which answer this thread's dynamic tool calls.
    pub(super) host_tools: Option<Arc<dyn HostTools>>,
    pub(super) child_streams: Option<ChildStreams>,
}
impl<A: Auth> ThreadActor<A> {
    #[expect(clippy::too_many_arguments)]
    pub(super) fn new(
        auth: A,
        client: SessionClient,
        thread: Arc<dyn CodexThreadImpl>,
        models_manager: Arc<dyn ModelsManagerImpl>,
        config: Config,
        message_rx: mpsc::UnboundedReceiver<ThreadMessage>,
        resolution_tx: mpsc::UnboundedSender<ThreadMessage>,
        resolution_rx: mpsc::UnboundedReceiver<ThreadMessage>,
    ) -> Self {
        Self {
            auth,
            client,
            thread,
            config,
            models_manager,
            resolution_tx,
            submissions: HashMap::new(),
            message_rx,
            resolution_rx,
            last_sent_config_options: None,
            host_tools: None,
            child_streams: None,
        }
    }

    pub(super) async fn spawn(mut self) {
        let mut message_rx_open = true;
        loop {
            tokio::select! {
                biased;
                message = self.message_rx.recv(), if message_rx_open => match message {
                    Some(message) => self.handle_message(message).await,
                    None => message_rx_open = false,
                },
                message = self.resolution_rx.recv() => if let Some(message) = message {
                    self.handle_message(message).await
                },
                event = self.thread.next_event() => match event {
                    Ok(event) => self.handle_event(event).await,
                    Err(e) => {
                        error!("Error getting next event: {:?}", e);
                        break;
                    }
                }
            }
            // Litter collection of senders with no receivers
            self.submissions
                .retain(|_, submission| submission.is_active());

            if !message_rx_open && self.submissions.is_empty() {
                break;
            }
        }
    }

    pub(super) async fn handle_message(&mut self, message: ThreadMessage) {
        match message {
            ThreadMessage::Load { response_tx } => {
                let result = self.handle_load().await;
                drop(response_tx.send(result));
                let client = self.client.clone();
                // Have this happen after the session is loaded by putting it
                // in a separate task
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    client.send_notification(SessionUpdate::AvailableCommandsUpdate(
                        AvailableCommandsUpdate::new(Self::builtin_commands()),
                    ));
                });
            }
            ThreadMessage::GetConfigOptions { response_tx } => {
                let result = self.config_options().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Prompt {
                request,
                response_tx,
            } => {
                let result = self.handle_prompt(request).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::SetMode { mode, response_tx } => {
                let result = self.handle_set_mode(mode).await;
                drop(response_tx.send(result));
                self.maybe_emit_config_options_update().await;
            }
            ThreadMessage::SetConfigOption {
                config_id,
                value,
                response_tx,
            } => {
                let result = self.handle_set_config_option(config_id, value).await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Cancel { response_tx } => {
                let result = self.handle_cancel().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::Shutdown { response_tx } => {
                let result = self.handle_shutdown().await;
                drop(response_tx.send(result));
            }
            ThreadMessage::ReplayHistory {
                history,
                response_tx,
            } => {
                let result = self.handle_replay_history(history);
                drop(response_tx.send(result));
            }
            ThreadMessage::PermissionRequestResolved {
                submission_id,
                interaction_id,
                request_key,
                response,
            } => {
                let Some(submission) = self.submissions.get_mut(&submission_id) else {
                    warn!(
                        "Ignoring permission response for unknown submission ID: {submission_id}"
                    );
                    return;
                };

                if let Err(err) = submission
                    .handle_permission_request_resolved(
                        &self.client,
                        interaction_id,
                        request_key,
                        response,
                    )
                    .await
                {
                    submission.detach_pending_interactions();
                    submission.fail(err);
                }
            }
        }
    }

    pub(super) async fn handle_load(&mut self) -> Result<LoadSessionResponse, Error> {
        Ok(LoadSessionResponse::new()
            .modes(self.modes())
            .config_options(self.config_options().await?))
    }

    pub(super) async fn handle_cancel(&mut self) -> Result<(), Error> {
        self.detach_pending_interactions();
        self.thread
            .submit(Op::Interrupt)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    pub(super) async fn handle_shutdown(&mut self) -> Result<(), Error> {
        self.detach_pending_interactions();
        self.thread
            .submit(Op::Shutdown)
            .await
            .map_err(|e| Error::from(anyhow::anyhow!(e)))?;
        Ok(())
    }

    pub(super) fn detach_pending_interactions(&mut self) {
        for submission in self.submissions.values_mut() {
            submission.detach_pending_interactions();
        }
    }

    pub(super) async fn handle_event(&mut self, Event { id, msg }: Event) {
        let child = match &msg {
            EventMsg::CollabAgentSpawnEnd(event) => event.new_thread_id,
            EventMsg::CollabResumeEnd(event) => Some(event.receiver_thread_id),
            _ => None,
        };
        if let Some(submission) = self.submissions.get_mut(&id) {
            submission.handle_event(&self.client, msg).await;
        } else {
            warn!("Received event for unknown submission ID: {id} {msg:?}");
        }
        if let (Some(child), Some(streams)) = (child, &mut self.child_streams) {
            streams
                .start(child, self.client.clone(), self.host_tools.clone())
                .await;
        }
    }
}
