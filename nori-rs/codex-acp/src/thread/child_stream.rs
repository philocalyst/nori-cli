//! Forward activity from Codex child threads on their own ACP session ids.

use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use codex_core::ThreadManager;
use codex_protocol::ThreadId;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::turn_input::TurnInputSubmission;
use nori_acp_kit::TurnGate;
use nori_acp_kit::TurnPolicy;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::ContentBlock;
use nori_acp_kit::schema::PromptRequest;
use nori_acp_kit::schema::SessionId;
use nori_acp_kit::schema::StopReason;
use nori_host_tools::HostTools;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tracing::warn;

use crate::thread::ThreadMessage;
use crate::thread::backends::CodexThreadImpl;
use crate::thread::client::SessionClient;
use crate::thread::prompt::PromptState;
use crate::thread::submit::build_prompt_items;
use crate::thread::submit::user_turn_op;

pub(crate) enum ChildPrompt {
    Prompt(PromptRequest, oneshot::Sender<Result<StopReason, Error>>),
    Cancel(oneshot::Sender<Result<(), Error>>),
    Close(oneshot::Sender<Result<(), Error>>),
}
pub(crate) type ChildPrompts = Arc<Mutex<HashMap<SessionId, mpsc::UnboundedSender<ChildPrompt>>>>;
type ChildCloseResponse = Option<(oneshot::Sender<Result<(), Error>>, Result<(), Error>)>;

pub(super) struct ChildStreams {
    manager: Arc<ThreadManager>,
    prompts: ChildPrompts,
    seen: HashSet<ThreadId>,
    tasks: JoinSet<()>,
}

impl ChildStreams {
    pub(super) fn new(manager: Arc<ThreadManager>, prompts: ChildPrompts) -> Self {
        Self {
            manager,
            prompts,
            seen: HashSet::new(),
            tasks: JoinSet::new(),
        }
    }

    pub(super) async fn start(
        &mut self,
        id: ThreadId,
        parent: SessionClient,
        host_tools: Option<Arc<dyn HostTools>>,
    ) {
        let session = SessionId::new(id.to_string());
        if self.seen.contains(&id)
            && self
                .prompts
                .lock()
                .unwrap()
                .get(&session)
                .is_some_and(|sender| !sender.is_closed())
        {
            return;
        }
        self.seen.insert(id);
        let thread = match self.manager.get_thread(id).await {
            Ok(thread) => thread,
            Err(error) => {
                warn!(%id, %error, "cannot subscribe to child thread");
                self.seen.remove(&id);
                return;
            }
        };
        let mut client = parent.clone();
        client.session_id = SessionId::new(id.to_string());
        let manager = Arc::clone(&self.manager);
        let prompts = Arc::clone(&self.prompts);
        let (sender, receiver) = mpsc::unbounded_channel();
        let child_session_id = client.session_id.clone();
        prompts
            .lock()
            .unwrap()
            .insert(client.session_id.clone(), sender);
        self.tasks.spawn(async move {
            let close_response = forward(
                thread,
                client.clone(),
                Some(parent),
                host_tools,
                Some((manager, Arc::clone(&prompts))),
                receiver,
            )
            .await;
            let mut prompts = prompts.lock().unwrap();
            if prompts
                .get(&child_session_id)
                .is_some_and(mpsc::UnboundedSender::is_closed)
            {
                prompts.remove(&child_session_id);
            }
            drop(prompts);
            if let Some((response_tx, result)) = close_response {
                drop(response_tx.send(result));
            }
        });
    }
}

fn forward(
    thread: Arc<dyn CodexThreadImpl>,
    client: SessionClient,
    parent: Option<SessionClient>,
    host_tools: Option<Arc<dyn HostTools>>,
    manager: Option<(Arc<ThreadManager>, ChildPrompts)>,
    mut prompt_rx: mpsc::UnboundedReceiver<ChildPrompt>,
) -> Pin<Box<dyn Future<Output = ChildCloseResponse> + Send>> {
    Box::pin(async move {
        let (resolution_tx, mut resolution_rx) = mpsc::unbounded_channel();
        let mut submissions: HashMap<String, PromptState> = HashMap::new();
        // Same start-or-steer rule as a root session.
        let gate = TurnGate::new(TurnPolicy::Steerable);
        let mut children = manager.map(|(manager, prompts)| ChildStreams::new(manager, prompts));
        loop {
            tokio::select! {
                Some(command) = prompt_rx.recv() => {
                    let (request, response_tx) = match command {
                        ChildPrompt::Prompt(request, response_tx) => (request, response_tx),
                        ChildPrompt::Cancel(response_tx) => {
                            gate.cancel();
                            let result = thread
                                .submit(Op::Interrupt)
                                .await
                                .map(|_| ())
                                .map_err(|error| Error::from(anyhow::anyhow!(error)));
                            drop(response_tx.send(result));
                            continue;
                        }
                        ChildPrompt::Close(response_tx) => {
                            gate.cancel();
                            let result = thread
                                .submit(Op::Interrupt)
                                .await
                                .map(|_| ())
                                .map_err(|error| Error::from(anyhow::anyhow!(error)));
                            for state in submissions.values_mut() {
                                state.detach_pending_interactions();
                                if let Some(response_tx) = state.response_tx.take() {
                                    drop(response_tx.send(Ok(StopReason::Cancelled)));
                                }
                            }
                            break Some((response_tx, result));
                        }
                    };
                    let text = request.prompt.iter().filter_map(|block| match block {
                        ContentBlock::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    }).collect::<Vec<_>>().join("\n");
                    let turn = match gate.begin().await {
                        Ok(turn) => turn,
                        Err(error) => {
                            drop(response_tx.send(Err(error.into())));
                            continue;
                        }
                    };
                    let items = build_prompt_items(request.prompt);
                    let mut replies = Vec::new();
                    let op = user_turn_op(items, &mut replies);
                    match thread.submit(op).await {
                        Ok(id) => {
                            let admission = match replies.pop() {
                                Some(reply) => reply.await.map_err(|error| error.to_string()).and_then(|result| result.map_err(|error| error.to_string())),
                                None => Err("missing child turn admission".to_owned()),
                            };
                            match admission {
                                Ok(TurnInputSubmission::Started { .. }) => {
                                    client.send_user_message(text);
                                    let mut state = PromptState::new(id.clone(), thread.clone(), resolution_tx.clone(), response_tx);
                                    state.host_tools = host_tools.clone();
                                    state.turn = Some(turn);
                                    submissions.insert(id, state);
                                }
                                Ok(TurnInputSubmission::Steered { .. } | TurnInputSubmission::Accepted) => {
                                    client.send_user_message(text);
                                    drop(response_tx.send(Ok(StopReason::EndTurn)));
                                }
                                Ok(TurnInputSubmission::NotSubmitted { reason }) => {
                                    drop(response_tx.send(Err(Error::internal_error().data(format!("child input not submitted: {reason:?}")))));
                                }
                                Err(error) => {
                                    drop(response_tx.send(Err(Error::internal_error().data(error))));
                                }
                            }
                        }
                        Err(error) => { drop(response_tx.send(Err(Error::internal_error().data(error.to_string())))); }
                    }
                }
                event = thread.next_event() => {
                    let Ok(Event { id, msg }) = event else {
                        for state in submissions.values_mut() {
                            state.detach_pending_interactions();
                            if let Some(response_tx) = state.response_tx.take() {
                                drop(response_tx.send(Ok(StopReason::Cancelled)));
                            }
                        }
                        break None;
                    };
                    let child = match &msg {
                        EventMsg::CollabAgentSpawnEnd(event) => event.new_thread_id,
                        EventMsg::CollabResumeEnd(event) => Some(event.receiver_thread_id),
                        _ => None,
                    };
                    let status = match &msg {
                        EventMsg::TurnStarted(_) => Some("running"),
                        EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_) => Some("turn_idle"),
                        // Codex can reopen a closed thread using the same id. Keep its history
                        // available for a later resume rather than permanently ending the node.
                        EventMsg::ShutdownComplete => Some("turn_idle"),
                        _ => None,
                    };
                    if let (Some(parent), Some(status)) = (&parent, status) {
                        let params = json!({
                            "sessionId": parent.session_id.to_string(),
                            "update": {
                                "sessionUpdate": "subagent_state_update",
                                "subagentSessionId": client.session_id.to_string(),
                                "state": status,
                            }
                        });
                        if let Err(error) = parent.client.send_untyped_notification(params) {
                            warn!(%error, "cannot report child state");
                        }
                    }
                    let shutdown = matches!(msg, EventMsg::ShutdownComplete);
                    let ended = matches!(msg, EventMsg::TurnComplete(_) | EventMsg::TurnAborted(_));
                    let state = submissions.entry(id.clone()).or_insert_with(|| {
                        let (response_tx, _response_rx) = oneshot::channel();
                        let mut state = PromptState::new(id.clone(), thread.clone(), resolution_tx.clone(), response_tx);
                        state.host_tools = host_tools.clone();
                        state
                    });
                    state.handle_event(&client, msg).await;
                    if ended {
                        submissions.remove(&id);
                    }
                    if let (Some(child), Some(children)) = (child, &mut children) {
                        children.start(child, client.clone(), host_tools.clone()).await;
                    }
                    if shutdown { break None; }
                }
                Some(ThreadMessage::PermissionRequestResolved { submission_id, interaction_id, request_key, response }) = resolution_rx.recv() => {
                    if let Some(state) = submissions.get_mut(&submission_id)
                        && let Err(error) = state.handle_permission_request_resolved(&client, interaction_id, request_key, response).await
                    {
                        warn!(%error, "child permission response failed");
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use codex_protocol::error::CodexErr;
    use codex_protocol::protocol::AgentMessageContentDeltaEvent;
    use codex_protocol::protocol::TurnAbortedEvent;
    use codex_protocol::protocol::TurnCompleteEvent;
    use nori_acp_kit::schema::RequestPermissionRequest;
    use nori_acp_kit::schema::RequestPermissionResponse;
    use nori_acp_kit::schema::SessionNotification;
    use nori_acp_kit::schema::SessionUpdate;
    use pretty_assertions::assert_eq;
    use tokio::sync::Mutex as AsyncMutex;

    use super::*;
    use crate::thread::client::ClientSender;

    struct EventThread {
        events: mpsc::UnboundedSender<Event>,
        event_rx: AsyncMutex<mpsc::UnboundedReceiver<Event>>,
    }

    impl CodexThreadImpl for EventThread {
        fn submit(
            &self,
            op: Op,
        ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
            Box::pin(async move {
                match op {
                    Op::TurnInput { reply, .. } => {
                        drop(reply.send(Ok(TurnInputSubmission::Started {
                            turn_id: "turn".into(),
                            root_turn_id: "turn".into(),
                        })));
                    }
                    Op::Interrupt => {
                        drop(self.events.send(Event {
                            id: "turn".into(),
                            msg: EventMsg::TurnAborted(TurnAbortedEvent {
                                turn_id: Some("turn".into()),
                                root_turn_id: None,
                                error: None,
                                started_at: None,
                                reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                                completed_at: None,
                                duration_ms: None,
                            }),
                        }));
                    }
                    _ => {}
                }
                Ok("turn".into())
            })
        }

        fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
            Box::pin(async { Ok(self.event_rx.lock().await.recv().await.expect("event")) })
        }
    }

    struct Capture {
        notifications: mpsc::UnboundedSender<SessionNotification>,
        updates: mpsc::UnboundedSender<serde_json::Value>,
    }

    impl ClientSender for Capture {
        fn send_session_notification(
            &self,
            notification: SessionNotification,
        ) -> Result<(), Error> {
            self.notifications.send(notification).unwrap();
            Ok(())
        }

        fn send_untyped_notification(&self, params: serde_json::Value) -> Result<(), Error> {
            self.updates.send(params).unwrap();
            Ok(())
        }

        fn request_permission(
            &self,
            _request: RequestPermissionRequest,
        ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>>
        {
            Box::pin(async { unreachable!() })
        }
    }

    #[tokio::test]
    async fn child_text_is_forwarded_on_the_child_session() {
        let (events, event_rx) = mpsc::unbounded_channel();
        let (notifications, mut notification_rx) = mpsc::unbounded_channel();
        let (updates, mut update_rx) = mpsc::unbounded_channel();
        let thread = Arc::new(EventThread {
            events: events.clone(),
            event_rx: AsyncMutex::new(event_rx),
        });
        let capture = Arc::new(Capture {
            notifications,
            updates,
        });
        let client =
            SessionClient::with_client(SessionId::new("child"), capture.clone(), Arc::default());
        let parent = SessionClient::with_client(SessionId::new("root"), capture, Arc::default());
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(forward(thread, client, Some(parent), None, None, prompt_rx));
        events
            .send(Event {
                id: "turn".into(),
                msg: EventMsg::AgentMessageContentDelta(AgentMessageContentDeltaEvent {
                    thread_id: "child".into(),
                    turn_id: "turn".into(),
                    item_id: "item".into(),
                    delta: "working".into(),
                }),
            })
            .unwrap();
        let notification = tokio::time::timeout(Duration::from_secs(2), notification_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(notification.session_id, SessionId::new("child"));
        let SessionUpdate::AgentMessageChunk(chunk) = notification.update else {
            panic!("expected message chunk")
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text")
        };
        assert_eq!(text.text, "working");
        let (response_tx, response_rx) = oneshot::channel();
        prompt_tx
            .send(ChildPrompt::Prompt(
                PromptRequest::new(SessionId::new("child"), vec!["follow up".into()]),
                response_tx,
            ))
            .unwrap();
        let echo = tokio::time::timeout(Duration::from_secs(2), notification_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(echo.session_id, SessionId::new("child"));
        let SessionUpdate::UserMessageChunk(chunk) = echo.update else {
            panic!("expected user message")
        };
        let ContentBlock::Text(text) = chunk.content else {
            panic!("expected text")
        };
        assert_eq!(text.text, "follow up");
        events
            .send(Event {
                id: "turn".into(),
                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                    last_agent_message: None,
                    root_turn_id: None,
                    error: None,
                    started_at: None,
                    turn_id: "turn".into(),
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                }),
            })
            .unwrap();
        let update = tokio::time::timeout(Duration::from_secs(2), update_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            update,
            json!({"sessionId":"root", "update": {"sessionUpdate":"subagent_state_update", "subagentSessionId":"child", "state":"turn_idle"}})
        );
        assert_eq!(response_rx.await.unwrap().unwrap(), StopReason::EndTurn);
        task.abort();
    }

    #[tokio::test]
    async fn child_cancel_interrupts_active_turn_and_completes_prompt_as_cancelled() {
        let (events, event_rx) = mpsc::unbounded_channel();
        let (notifications, mut notification_rx) = mpsc::unbounded_channel();
        let (updates, _update_rx) = mpsc::unbounded_channel();
        let thread = Arc::new(EventThread {
            events: events.clone(),
            event_rx: AsyncMutex::new(event_rx),
        });
        let capture = Arc::new(Capture {
            notifications,
            updates,
        });
        let client = SessionClient::with_client(SessionId::new("child"), capture, Arc::default());
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(forward(thread, client, None, None, None, prompt_rx));
        let (prompt_response_tx, prompt_response_rx) = oneshot::channel();
        prompt_tx
            .send(ChildPrompt::Prompt(
                PromptRequest::new(SessionId::new("child"), vec!["work".into()]),
                prompt_response_tx,
            ))
            .unwrap();
        let _user_echo = tokio::time::timeout(Duration::from_secs(2), notification_rx.recv())
            .await
            .unwrap()
            .unwrap();

        let (cancel_response_tx, cancel_response_rx) = oneshot::channel();
        prompt_tx
            .send(ChildPrompt::Cancel(cancel_response_tx))
            .unwrap();
        cancel_response_rx.await.unwrap().unwrap();
        assert_eq!(
            prompt_response_rx.await.unwrap().unwrap(),
            StopReason::Cancelled
        );
        task.abort();
    }

    #[tokio::test]
    async fn cancelling_idle_child_session_is_idempotent() {
        let (events, event_rx) = mpsc::unbounded_channel();
        let (notifications, _notification_rx) = mpsc::unbounded_channel();
        let (updates, _update_rx) = mpsc::unbounded_channel();
        let thread = Arc::new(EventThread {
            events,
            event_rx: AsyncMutex::new(event_rx),
        });
        let client = SessionClient::with_client(
            SessionId::new("child"),
            Arc::new(Capture {
                notifications,
                updates,
            }),
            Arc::default(),
        );
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(forward(thread, client, None, None, None, prompt_rx));

        for _ in 0..2 {
            let (response_tx, response_rx) = oneshot::channel();
            prompt_tx.send(ChildPrompt::Cancel(response_tx)).unwrap();
            response_rx.await.unwrap().unwrap();
        }
        task.abort();
    }

    #[tokio::test]
    async fn closing_child_session_cancels_active_prompt_and_finishes_close() {
        let (events, event_rx) = mpsc::unbounded_channel();
        let (notifications, mut notification_rx) = mpsc::unbounded_channel();
        let (updates, _update_rx) = mpsc::unbounded_channel();
        let thread = Arc::new(EventThread {
            events: events.clone(),
            event_rx: AsyncMutex::new(event_rx),
        });
        let client = SessionClient::with_client(
            SessionId::new("child"),
            Arc::new(Capture {
                notifications,
                updates,
            }),
            Arc::default(),
        );
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let close_response = forward(thread, client, None, None, None, prompt_rx).await;
            if let Some((response_tx, result)) = close_response {
                drop(response_tx.send(result));
            }
        });
        let (prompt_response_tx, prompt_response_rx) = oneshot::channel();
        prompt_tx
            .send(ChildPrompt::Prompt(
                PromptRequest::new(SessionId::new("child"), vec!["work".into()]),
                prompt_response_tx,
            ))
            .unwrap();
        let _user_echo = tokio::time::timeout(Duration::from_secs(2), notification_rx.recv())
            .await
            .unwrap()
            .unwrap();

        let (close_response_tx, close_response_rx) = oneshot::channel();
        prompt_tx
            .send(ChildPrompt::Close(close_response_tx))
            .unwrap();
        close_response_rx.await.unwrap().unwrap();
        assert_eq!(
            prompt_response_rx.await.unwrap().unwrap(),
            StopReason::Cancelled
        );
        task.await.unwrap();
    }
}
