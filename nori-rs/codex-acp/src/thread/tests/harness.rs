//! The stubs the flow tests run against: a recording Codex thread, client and models.

use codex_core::config::ConfigOverrides;
use codex_protocol::protocol::TurnStartedEvent;
use itertools::Itertools;
use nori_acp_kit::schema::SessionNotification;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicUsize;

use crate::thread::ThreadMessage;
use crate::thread::actor::ThreadActor;
use crate::thread::backends::Auth;
use crate::thread::backends::CodexThreadImpl;
use crate::thread::backends::ModelsManagerImpl;
use crate::thread::client::ClientSender;
use crate::thread::client::SessionClient;
use crate::thread::tests::flows::image_generation_test_saved_path;
use codex_core::config::Config;
use codex_core::test_support::all_model_presets;
use codex_core_api::TurnInput;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ModeKind;
use codex_protocol::error::CodexErr;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::parse_command::ParsedCommand;
use codex_protocol::protocol::AgentMessageContentDeltaEvent;
use codex_protocol::protocol::AgentMessageEvent;
use codex_protocol::protocol::ElicitationAction;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecApprovalRequestEvent;
use codex_protocol::protocol::ExecCommandBeginEvent;
use codex_protocol::protocol::ExecCommandEndEvent;
use codex_protocol::protocol::ExecCommandStatus;
use codex_protocol::protocol::ExitedReviewModeEvent;
use codex_protocol::protocol::ImageGenerationBeginEvent;
use codex_protocol::protocol::ImageGenerationEndEvent;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ReviewDecision;
use codex_protocol::protocol::ReviewOutputEvent;
use codex_protocol::protocol::ReviewRequest;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::user_input::UserInput;
use codex_utils_path_uri::LegacyAppPathString;
use codex_utils_path_uri::PathUri;
use nori_acp_kit::acp::Error;
use nori_acp_kit::schema::RequestPermissionOutcome;
use nori_acp_kit::schema::RequestPermissionRequest;
use nori_acp_kit::schema::RequestPermissionResponse;
use nori_acp_kit::schema::SessionId;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::mpsc::UnboundedSender;

pub(super) async fn setup() -> anyhow::Result<(
    SessionId,
    Arc<StubClient>,
    Arc<StubCodexThread>,
    UnboundedSender<ThreadMessage>,
    tokio::task::JoinHandle<()>,
)> {
    let session_id = SessionId::new("test");
    let client = Arc::new(StubClient::new());
    let session_client =
        SessionClient::with_client(session_id.clone(), client.clone(), Arc::default());
    let conversation = Arc::new(StubCodexThread::new());
    let models_manager = Arc::new(StubModelsManager);
    let config =
        Config::load_with_cli_overrides_and_harness_overrides(vec![], ConfigOverrides::default())
            .await?;
    let (message_tx, message_rx) = tokio::sync::mpsc::unbounded_channel();
    let (resolution_tx, resolution_rx) = tokio::sync::mpsc::unbounded_channel();

    let actor = ThreadActor::new(
        StubAuth,
        session_client,
        conversation.clone(),
        models_manager,
        config,
        message_rx,
        resolution_tx,
        resolution_rx,
    );

    let handle = tokio::spawn(actor.spawn());
    Ok((session_id, client, conversation, message_tx, handle))
}

pub(super) struct StubAuth;

impl Auth for StubAuth {
    async fn logout(&self) -> Result<bool, Error> {
        Ok(true)
    }
}

pub(super) struct StubModelsManager;

impl ModelsManagerImpl for StubModelsManager {
    fn get_model(
        &self,
        _model_id: &Option<String>,
    ) -> Pin<Box<dyn Future<Output = String> + Send + '_>> {
        Box::pin(async { all_model_presets()[0].to_owned().id })
    }

    fn list_models(&self) -> Pin<Box<dyn Future<Output = Vec<ModelPreset>> + Send + '_>> {
        Box::pin(async { all_model_presets().to_owned() })
    }
}

/// The parts of a submitted `Op` that the tests assert on. `Op` itself is neither `Clone` nor
/// `PartialEq` since Codex 0.169 (several variants carry a reply channel), so the stub records
/// this projection instead.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Recorded {
    UserInput(Vec<UserInput>),
    Compact,
    Review(ReviewRequest),
    ExecApproval {
        id: String,
        turn_id: Option<String>,
        decision: ReviewDecision,
    },
    ResolveElicitation {
        server_name: String,
        request_id: codex_protocol::mcp::RequestId,
        decision: ElicitationAction,
        content: Option<serde_json::Value>,
        meta: Option<serde_json::Value>,
    },
    UserInputAnswer {
        id: String,
        response: codex_protocol::request_user_input::RequestUserInputResponse,
    },
    Interrupt,
    Shutdown,
    Other(String),
}

impl Recorded {
    pub(super) fn from_op(op: &Op) -> Self {
        match op {
            Op::TurnInput { request, .. } => match &request.input {
                TurnInput::UserInput { content, .. } => Self::UserInput(content.clone()),
                other => Self::Other(format!("{other:?}")),
            },
            Op::Compact => Self::Compact,
            Op::Review { review_request } => Self::Review(review_request.clone()),
            Op::ExecApproval {
                id,
                turn_id,
                decision,
            } => Self::ExecApproval {
                id: id.clone(),
                turn_id: turn_id.clone(),
                decision: decision.clone(),
            },
            Op::ResolveElicitation {
                server_name,
                request_id,
                decision,
                content,
                meta,
            } => Self::ResolveElicitation {
                server_name: server_name.clone(),
                request_id: request_id.clone(),
                decision: *decision,
                content: content.clone(),
                meta: meta.clone(),
            },
            Op::UserInputAnswer { id, response } => Self::UserInputAnswer {
                id: id.clone(),
                response: response.clone(),
            },
            Op::Interrupt => Self::Interrupt,
            Op::Shutdown => Self::Shutdown,
            other => Self::Other(format!("{other:?}")),
        }
    }
}

pub(super) struct StubCodexThread {
    pub(super) current_id: AtomicUsize,
    pub(super) active_prompt_id: std::sync::Mutex<Option<String>>,
    pub(super) ops: std::sync::Mutex<Vec<Recorded>>,
    pub(super) op_tx: mpsc::UnboundedSender<Event>,
    pub(super) op_rx: Mutex<mpsc::UnboundedReceiver<Event>>,
}

impl StubCodexThread {
    pub(super) fn new() -> Self {
        let (op_tx, op_rx) = mpsc::unbounded_channel();
        StubCodexThread {
            current_id: AtomicUsize::new(0),
            active_prompt_id: std::sync::Mutex::default(),
            ops: std::sync::Mutex::default(),
            op_tx,
            op_rx: Mutex::new(op_rx),
        }
    }
}

impl CodexThreadImpl for StubCodexThread {
    fn submit(
        &self,
        op: Op,
    ) -> Pin<Box<dyn Future<Output = Result<String, CodexErr>> + Send + '_>> {
        Box::pin(async move {
            let id = self
                .current_id
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

            self.ops.lock().unwrap().push(Recorded::from_op(&op));

            match op {
                Op::TurnInput { request, reply, .. } => {
                    let TurnInput::UserInput { content: items, .. } = request.input else {
                        unimplemented!("only user input turns are stubbed")
                    };
                    // Core accepts every turn input in these tests by starting a new turn.
                    reply
                        .send(Ok(TurnInputSubmission::Started {
                            turn_id: id.to_string(),
                            root_turn_id: id.to_string(),
                        }))
                        .ok();
                    *self.active_prompt_id.lock().unwrap() = Some(id.to_string());
                    let prompt = items
                        .into_iter()
                        .map(|i| match i {
                            UserInput::Text { text, .. } => text,
                            _ => unimplemented!(),
                        })
                        .join("\n");

                    if prompt == "parallel-exec" {
                        // Emit interleaved exec events: Begin A, Begin B, End A, End B
                        let turn_id = id.to_string();
                        let cwd = std::env::current_dir().unwrap();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                            call_id: "call-a".into(),
                            process_id: None,
                            plugin_id: None,
                            script_path: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "a".into()],
                            cwd: PathUri::from_host_native_path(&cwd).unwrap(),
                            parsed_cmd: vec![ParsedCommand::Unknown {
                                cmd: "echo a".into(),
                            }],
                            source: Default::default(),
                            interaction_input: None,
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandBegin(ExecCommandBeginEvent {
                            call_id: "call-b".into(),
                            process_id: None,
                            plugin_id: None,
                            script_path: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "b".into()],
                            cwd: PathUri::from_host_native_path(&cwd).unwrap(),
                            parsed_cmd: vec![ParsedCommand::Unknown {
                                cmd: "echo b".into(),
                            }],
                            source: Default::default(),
                            interaction_input: None,
                            started_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "call-a".into(),
                            process_id: None,
                            plugin_id: None,
                            script_path: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "a".into()],
                            cwd: PathUri::from_host_native_path(&cwd).unwrap(),
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            aggregated_output: "a\n".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::ExecCommandEnd(ExecCommandEndEvent {
                            call_id: "call-b".into(),
                            process_id: None,
                            plugin_id: None,
                            script_path: None,
                            turn_id: turn_id.clone(),
                            command: vec!["echo".into(), "b".into()],
                            cwd: PathUri::from_host_native_path(&cwd).unwrap(),
                            parsed_cmd: vec![],
                            source: Default::default(),
                            interaction_input: None,
                            aggregated_output: "b\n".into(),
                            exit_code: 0,
                            duration: std::time::Duration::from_millis(10),
                            status: ExecCommandStatus::Completed,
                            completed_at_ms: 0,
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            root_turn_id: None,
                            error: None,
                            started_at: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "image-generation" {
                        let turn_id = id.to_string();
                        let saved_path = image_generation_test_saved_path();
                        let send = |msg| {
                            self.op_tx
                                .send(Event {
                                    id: id.to_string(),
                                    msg,
                                })
                                .unwrap();
                        };
                        send(EventMsg::ImageGenerationBegin(ImageGenerationBeginEvent {
                            call_id: "ig-1".into(),
                        }));
                        send(EventMsg::ImageGenerationEnd(ImageGenerationEndEvent {
                            call_id: "ig-1".into(),
                            status: "completed".into(),
                            revised_prompt: Some("A tiny blue square".into()),
                            image: serde_json::from_value(serde_json::json!({ "result": "Zm9v" }))
                                .unwrap(),
                            transparent_background: None,
                            failure: None,
                            saved_path: Some(saved_path.try_into()?),
                            input_images: Vec::new(),
                            generation_id: None,
                        }));
                        send(EventMsg::TurnComplete(TurnCompleteEvent {
                            last_agent_message: None,
                            root_turn_id: None,
                            error: None,
                            started_at: None,
                            turn_id,
                            completed_at: None,
                            duration_ms: None,
                            time_to_first_token_ms: None,
                        }));
                    } else if prompt == "thread-goal-update" {
                        let turn_id = id.to_string();
                        let thread_id = ThreadId::default();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
                                    thread_id,
                                    turn_id: Some(turn_id.clone()),
                                    goal: ThreadGoal {
                                        thread_id,
                                        objective: "Ship the goal update".to_string(),
                                        status: ThreadGoalStatus::Active,
                                        token_budget: Some(100),
                                        tokens_used: 10,
                                        time_used_seconds: 2,
                                        created_at: 1,
                                        updated_at: 2,
                                    },
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    root_turn_id: None,
                                    error: None,
                                    started_at: None,
                                    turn_id,
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    } else if prompt == "approval-block" {
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::ExecApprovalRequest(ExecApprovalRequestEvent {
                                    call_id: "call-id".to_string(),
                                    approval_id: Some("approval-id".to_string()),
                                    kind: Default::default(),
                                    model_context: None,
                                    plugin_id: None,
                                    script_path: None,
                                    environment_id: None,
                                    turn_id: id.to_string(),
                                    started_at_ms: 0,
                                    command: vec!["echo".to_string(), "hi".to_string()],
                                    cwd: LegacyAppPathString::from_path(
                                        &std::env::current_dir().unwrap(),
                                    ),
                                    reason: None,
                                    network_approval_context: None,
                                    proposed_execpolicy_amendment: None,
                                    proposed_network_policy_amendments: None,
                                    additional_permissions: None,
                                    available_decisions: Some(vec![
                                        ReviewDecision::Approved,
                                        ReviewDecision::Abort,
                                    ]),
                                    parsed_cmd: vec![ParsedCommand::Unknown {
                                        cmd: "echo hi".to_string(),
                                    }],
                                }),
                            })
                            .unwrap();
                    } else {
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessageContentDelta(
                                    AgentMessageContentDeltaEvent {
                                        thread_id: id.to_string(),
                                        turn_id: id.to_string(),
                                        item_id: id.to_string(),
                                        delta: prompt.clone(),
                                    },
                                ),
                            })
                            .unwrap();
                        // Send non-delta event (should be deduplicated, but handled by deduplication)
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::AgentMessage(AgentMessageEvent {
                                    message: prompt,
                                    phase: None,
                                    memory_citation: None,
                                    moderation: None,
                                    delivery: None,
                                    questions: None,
                                }),
                            })
                            .unwrap();
                        self.op_tx
                            .send(Event {
                                id: id.to_string(),
                                msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                    last_agent_message: None,
                                    root_turn_id: None,
                                    error: None,
                                    started_at: None,
                                    turn_id: id.to_string(),
                                    completed_at: None,
                                    duration_ms: None,
                                    time_to_first_token_ms: None,
                                }),
                            })
                            .unwrap();
                    }
                }
                Op::Compact => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnStarted(TurnStartedEvent {
                                model_context_window: None,
                                turn_attribution: None,
                                root_turn_id: None,
                                collaboration_mode_kind: ModeKind::default(),
                                turn_id: id.to_string(),
                                trace_id: None,
                                started_at: None,
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::AgentMessage(AgentMessageEvent {
                                message: "Compact task completed".to_string(),
                                phase: None,
                                memory_citation: None,
                                moderation: None,
                                delivery: None,
                                questions: None,
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                last_agent_message: None,
                                root_turn_id: None,
                                error: None,
                                started_at: None,
                                turn_id: id.to_string(),
                                completed_at: None,
                                duration_ms: None,
                                time_to_first_token_ms: None,
                            }),
                        })
                        .unwrap();
                }
                Op::Review { review_request } => {
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::EnteredReviewMode(
                                codex_protocol::protocol::EnteredReviewModeEvent {
                                    target: review_request.target.clone(),
                                    user_facing_hint: review_request.user_facing_hint.clone(),
                                    turn_id: None,
                                    item_id: None,
                                },
                            ),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::ExitedReviewMode(ExitedReviewModeEvent {
                                turn_id: None,
                                item_id: None,
                                review_output: Some(ReviewOutputEvent {
                                    findings: vec![],
                                    overall_correctness: String::new(),
                                    overall_explanation: review_request
                                        .user_facing_hint
                                        .clone()
                                        .unwrap_or_default(),
                                    overall_confidence_score: 1.,
                                }),
                            }),
                        })
                        .unwrap();
                    self.op_tx
                        .send(Event {
                            id: id.to_string(),
                            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                                last_agent_message: None,
                                root_turn_id: None,
                                error: None,
                                started_at: None,
                                turn_id: id.to_string(),
                                completed_at: None,
                                duration_ms: None,
                                time_to_first_token_ms: None,
                            }),
                        })
                        .unwrap();
                }
                Op::ExecApproval { .. }
                | Op::ResolveElicitation { .. }
                | Op::RequestPermissionsResponse { .. }
                | Op::PatchApproval { .. }
                | Op::UserInputAnswer { .. }
                | Op::Interrupt => {}
                Op::Shutdown => {
                    if let Some(active_prompt_id) = self.active_prompt_id.lock().unwrap().take() {
                        self.op_tx
                            .send(Event {
                                id: active_prompt_id.clone(),
                                msg: EventMsg::TurnAborted(TurnAbortedEvent {
                                    turn_id: Some(active_prompt_id),
                                    root_turn_id: None,
                                    error: None,
                                    started_at: None,
                                    reason: codex_protocol::protocol::TurnAbortReason::Interrupted,
                                    completed_at: None,
                                    duration_ms: None,
                                }),
                            })
                            .unwrap();
                    }
                }
                _ => {
                    unimplemented!()
                }
            }
            Ok(id.to_string())
        })
    }

    fn next_event(&self) -> Pin<Box<dyn Future<Output = Result<Event, CodexErr>> + Send + '_>> {
        Box::pin(async {
            let Some(event) = self.op_rx.lock().await.recv().await else {
                return Err(CodexErr::InternalAgentDied);
            };
            Ok(event)
        })
    }
}

pub(super) struct StubClient {
    pub(super) notifications: std::sync::Mutex<Vec<SessionNotification>>,
    pub(super) permission_requests: std::sync::Mutex<Vec<RequestPermissionRequest>>,
    pub(super) permission_responses: std::sync::Mutex<VecDeque<RequestPermissionResponse>>,
    pub(super) block_permission_requests: Option<Arc<Notify>>,
    pub(super) question_requests: std::sync::Mutex<Vec<serde_json::Value>>,
    pub(super) question_response: std::sync::Mutex<Option<serde_json::Value>>,
}

impl StubClient {
    pub(super) fn new() -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::default(),
            block_permission_requests: None,
            question_requests: std::sync::Mutex::default(),
            question_response: std::sync::Mutex::default(),
        }
    }

    pub(super) fn with_permission_responses(responses: Vec<RequestPermissionResponse>) -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::new(responses.into()),
            block_permission_requests: None,
            question_requests: std::sync::Mutex::default(),
            question_response: std::sync::Mutex::default(),
        }
    }

    pub(super) fn with_blocked_permission_requests(
        responses: Vec<RequestPermissionResponse>,
        notify: Arc<Notify>,
    ) -> Self {
        StubClient {
            notifications: std::sync::Mutex::default(),
            permission_requests: std::sync::Mutex::default(),
            permission_responses: std::sync::Mutex::new(responses.into()),
            block_permission_requests: Some(notify),
            question_requests: std::sync::Mutex::default(),
            question_response: std::sync::Mutex::default(),
        }
    }
}

impl ClientSender for StubClient {
    fn send_session_notification(&self, args: SessionNotification) -> Result<(), Error> {
        self.notifications.lock().unwrap().push(args);
        Ok(())
    }

    fn send_untyped_notification(&self, _params: serde_json::Value) -> Result<(), Error> {
        Ok(())
    }

    fn request_user_input(
        &self,
        params: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, Error>> + Send + '_>> {
        Box::pin(async move {
            self.question_requests.lock().unwrap().push(params);
            self.question_response
                .lock()
                .unwrap()
                .take()
                .ok_or_else(Error::method_not_found)
        })
    }

    fn request_permission(
        &self,
        args: RequestPermissionRequest,
    ) -> Pin<Box<dyn Future<Output = Result<RequestPermissionResponse, Error>> + Send + '_>> {
        Box::pin(async move {
            self.permission_requests.lock().unwrap().push(args);
            if let Some(notify) = &self.block_permission_requests {
                notify.notified().await;
            }
            Ok(self
                .permission_responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
                }))
        })
    }
}
