//! Replacing a session's agent in the middle of a conversation.
//!
//! The runtime owns the session's backend, so it is the one place that can swap it: the new
//! agent is started with the conversation's recorder and a checkpoint of the visible
//! conversation, shown to the front end only once it works (as a route change followed by its
//! own session start, so headers and status read from the usual events), and only then is the
//! old agent retired. A route that cannot start is skipped; when none can, the old agent
//! stays and the failure is reported.
//!
//! The swap runs with the old session's ledger fenced, so no prompt, wake or retry starts in the
//! middle of it; prompts that arrive meanwhile wait in the queue and run on the new agent.

use std::sync::Arc;
use std::time::Duration;

use nori_protocol::NoriEvent;
use nori_protocol::RequestFailure;
use nori_protocol::RequestFailureKind;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;

use super::PreparedAgent;
use super::build_backend_config;
use super::prompt_queue::PromptJob;
use super::prompt_queue::PromptQueue;
use crate::backend::AcpBackend;
use crate::backend::BackendEvent;
use crate::backend::PromptAdmission;
use crate::recovery::handoff::HandoffRequest;
use crate::session_event_fanout::SessionEventFanout;

/// How long a replacement agent may take to start and open its session.
const START_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the old agent's own processes get to stop.
const RETIRE_GRACE: Duration = Duration::from_secs(5);
/// How long to wait for the turn that just failed (and anything queued behind it) to finish
/// before the swap can fence the session.
const FENCE_WAIT: Duration = Duration::from_secs(30);

/// Everything the command loop owns that a replacement touches.
pub(super) struct Live<'a> {
    pub(super) backend: &'a mut Arc<AcpBackend>,
    pub(super) prompt_queue: &'a mut PromptQueue,
    pub(super) requests: &'a mpsc::UnboundedSender<HandoffRequest>,
    /// The channel every backend of this session reports to.
    pub(super) events: &'a mpsc::Sender<BackendEvent>,
    pub(super) fanout: &'a SessionEventFanout,
    /// Where the session's recorder is read from; follows the running backend.
    pub(super) recorder_cell: &'a std::sync::Mutex<
        Arc<tokio::sync::RwLock<Option<Arc<crate::transcript::TranscriptRecorder>>>>,
    >,
}

struct Accepted {
    changed: nori_protocol::RouteChanged,
    options: Vec<acp::SessionConfigOption>,
    session_id: acp::SessionId,
}

/// Hold a new backend's events until it is known to work: then the route change goes first,
/// then what the backend said while starting, then everything it says from then on.
fn gate(
    mut from_backend: mpsc::Receiver<BackendEvent>,
    to_front_end: mpsc::Sender<BackendEvent>,
) -> oneshot::Sender<Option<Accepted>> {
    let (verdict_tx, mut verdict_rx) = oneshot::channel::<Option<Accepted>>();
    tokio::spawn(async move {
        let mut held = Vec::new();
        let verdict = loop {
            tokio::select! {
                event = from_backend.recv() => match event {
                    Some(event) => held.push(event),
                    None => break verdict_rx.await.ok().flatten(),
                },
                verdict = &mut verdict_rx => break verdict.ok().flatten(),
            }
        };
        let Some(accepted) = verdict else {
            return;
        };
        let cause = accepted.changed.cause;
        // The new session continues the conversation: its start says so, so the front end
        // updates in place and shows only the route change.
        let public = |event| BackendEvent::Public(event);
        let mark = move |mut event: BackendEvent| {
            let BackendEvent::Public(SessionEvent::Nori(NoriEvent::SessionStarted(started))) =
                &mut event
            else {
                return event;
            };
            started.swap = Some(cause);
            event
        };
        let _ = to_front_end
            .send(public(SessionEvent::Nori(NoriEvent::RouteChanged(
                accepted.changed,
            ))))
            .await;
        for event in held {
            let _ = to_front_end.send(mark(event)).await;
        }
        // The session started before its model was settled; say what it settled on.
        let _ = to_front_end
            .send(public(SessionEvent::Acp(
                nori_protocol::AcpEvent::Notification(acp::AgentNotification::SessionNotification(
                    acp::SessionNotification::new(
                        accepted.session_id,
                        acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                            accepted.options,
                        )),
                    ),
                )),
            )))
            .await;
        while let Some(event) = from_backend.recv().await {
            if to_front_end.send(mark(event)).await.is_err() {
                break;
            }
        }
    });
    verdict_tx
}

enum Outcome {
    /// The new agent runs the session; `pending` are prompts that were queued, never admitted.
    Replaced(Arc<AcpBackend>, Vec<PromptJob>),
    /// No route started; why, route by route.
    NoRoute(String),
    /// The user cancelled before any route took over: nothing is half-started.
    Cancelled,
}

/// Resolves once the user has cancelled the swap.
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow_and_update() {
        if cancel.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// A start that was given up on still finishes; stop what it started.
fn retire_when_started(start: tokio::task::JoinHandle<Result<AcpBackend, String>>) {
    tokio::spawn(async move {
        if let Ok(Ok(backend)) = start.await {
            Arc::new(backend).retire(RETIRE_GRACE).await;
        }
    });
}

/// Start the first route of the plan that works, and retire `old` in its favour. Runs with
/// `old` fenced.
async fn replace_fenced(
    old: &Arc<AcpBackend>,
    prompt_queue: &mut PromptQueue,
    request: &HandoffRequest,
    requests: &mpsc::UnboundedSender<HandoffRequest>,
    events: &mpsc::Sender<BackendEvent>,
    recorder_cell: &std::sync::Mutex<
        Arc<tokio::sync::RwLock<Option<Arc<crate::transcript::TranscriptRecorder>>>>,
    >,
    cancel: &mut watch::Receiver<bool>,
) -> anyhow::Result<Outcome> {
    let template = old.handoff_template().await;
    let context = match &template.recorder {
        Some(recorder) => recorder.visible_context().await.ok(),
        None => None,
    };
    let total = i64::try_from(request.plan.len()).unwrap_or(i64::MAX);
    let mut skipped: Vec<String> = Vec::new();
    for (index, route) in request.plan.iter().enumerate() {
        let Some(agent) = route.agent.clone() else {
            skipped.push("a route names no agent".to_owned());
            continue;
        };
        let mut config = (*template.config).clone();
        config.active_agent = agent.clone();
        config.agent = agent.clone();
        if let Some(model) = &route.model {
            config.default_models.insert(agent.clone(), model.clone());
        }
        let mut backend_config = build_backend_config(super::AgentPrepareSpec {
            config: Arc::new(config),
            cli_version: template.cli_version.clone(),
            session_context: template.session_context.clone(),
            initial_context: context.clone(),
            handoff: template.recorder.clone(),
        });
        if template.seed.is_child() {
            // A delegated session stays where it is in its tree and keeps its role.
            backend_config.delegation = crate::backend::orchestrator::Seed {
                config: Arc::clone(&backend_config.delegation.config),
                ..template.seed.clone()
            };
        }
        let (events_tx, events_rx) = mpsc::channel(256);
        let verdict = gate(events_rx, events.clone());
        let mut start = tokio::spawn(async move {
            let prepared = PreparedAgent::prepare(backend_config)
                .await
                .map_err(|failure| format!("{:#}", failure.error))?;
            AcpBackend::spawn(prepared, events_tx)
                .await
                .map_err(|error| format!("{error:#}"))
        });
        let started = tokio::select! {
            started = tokio::time::timeout(START_TIMEOUT, &mut start) => started,
            () = cancelled(cancel) => {
                // Whatever is starting is stopped as soon as it exists; the session stays put.
                retire_when_started(start);
                return Ok(Outcome::Cancelled);
            }
        };
        let new = match started {
            Ok(Ok(Ok(new))) => Arc::new(new),
            Ok(Ok(Err(error))) => {
                skipped.push(format!("{agent}: {error}"));
                continue;
            }
            Ok(Err(error)) => {
                skipped.push(format!("{agent}: {error}"));
                continue;
            }
            Err(_) => {
                retire_when_started(start);
                skipped.push(format!("{agent}: timed out starting"));
                continue;
            }
        };
        new.bind_program_runtime();
        new.bind_handoff(requests.clone());
        if route.model.is_some()
            && let Err(error) = new.switch_to_route(route).await
        {
            skipped.push(format!("{agent}: {error:#}"));
            new.retire(RETIRE_GRACE).await;
            continue;
        }
        let session_id = new.session_id().await;
        let accounts = crate::recovery::AccountHeadroom::load(&template.config.nori_home);
        let _ = verdict.send(Some(Accepted {
            changed: nori_protocol::RouteChanged {
                from: request.from.clone(),
                to: new.current_route(),
                cause: request.cause,
                step: nori_protocol::FallbackStep {
                    index: i64::try_from(index).unwrap_or(0) + 1,
                    total,
                },
                detail: request.detail.clone(),
                from_account: Some(accounts.describe(&request.from)),
                to_account: Some(accounts.describe(&new.current_route())),
            },
            options: new.config_options(),
            session_id: session_id.clone(),
        }));
        if let Some(recorder) = &template.recorder
            && let Err(error) = recorder
                .record_backend_bound(
                    serde_json::json!({
                        "agent": agent,
                        "acp_session_id": session_id.to_string(),
                    })
                    .to_string(),
                )
                .await
        {
            tracing::warn!(%error, "failed to record the backend binding after a route change");
        }
        let pending = prompt_queue.take_pending().await;
        old.retire(RETIRE_GRACE).await;
        *recorder_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = new.transcript_recorder_cell();
        return Ok(Outcome::Replaced(new, pending));
    }
    Ok(Outcome::NoRoute(if skipped.is_empty() {
        "the policy names none".to_owned()
    } else {
        skipped.join("; ")
    }))
}

/// Replace the session's agent as `request` asks, then send its prompt again, while the
/// command loop keeps listening: a cancel (or a shutdown) abandons the swap at once.
pub(super) async fn run(
    live: Live<'_>,
    request: HandoffRequest,
    commands: &mut mpsc::UnboundedReceiver<super::HarnessCommand>,
    pending: &mut std::collections::VecDeque<super::HarnessCommand>,
) {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut acknowledge = Vec::new();
    let swap = replace(live, request, cancel_rx);
    tokio::pin!(swap);
    loop {
        tokio::select! {
            () = &mut swap => break,
            command = commands.recv() => match command {
                Some(super::HarnessCommand::Cancel { response_tx }) => {
                    cancel_tx.send_replace(true);
                    acknowledge.push(response_tx);
                }
                Some(shutdown @ super::HarnessCommand::Shutdown { .. }) => {
                    cancel_tx.send_replace(true);
                    pending.push_back(shutdown);
                }
                Some(other) => pending.push_back(other),
                None => {
                    cancel_tx.send_replace(true);
                }
            },
        }
    }
    for response_tx in acknowledge {
        let _ = response_tx.send(Ok(()));
    }
}

async fn replace(live: Live<'_>, request: HandoffRequest, mut cancel: watch::Receiver<bool>) {
    // The turn that just failed is still closing; the swap needs the session idle.
    let deadline = tokio::time::Instant::now() + FENCE_WAIT;
    let outcome = loop {
        let attempt = live
            .backend
            .while_fenced(
                "replace the agent",
                replace_fenced(
                    live.backend,
                    live.prompt_queue,
                    &request,
                    live.requests,
                    live.events,
                    live.recorder_cell,
                    &mut cancel,
                ),
            )
            .await;
        match attempt {
            Ok(outcome) => break outcome,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_millis(25)) => {}
                    () = cancelled(&mut cancel) => break Outcome::Cancelled,
                }
            }
            Err(error) => break Outcome::NoRoute(format!("{error:#}")),
        }
    };
    match outcome {
        Outcome::Replaced(new, queued) => {
            *live.backend = new;
            *live.prompt_queue = PromptQueue::start(Arc::clone(live.backend));
            // The user cancelled after the new agent took over: the prompt is not sent again.
            let cancelled_late = *cancel.borrow();
            if cancelled_late {
                notice(
                    live.fanout,
                    "Cancelled: the prompt was not sent again on the new route.",
                );
            }
            if let Some(prompt) = request.prompt.filter(|_| !cancelled_late) {
                let mut meta = prompt.meta.unwrap_or_default();
                // The retry answers under the request ID the client is waiting on.
                meta.insert(
                    nori_protocol::HANDOFF_RETRY_META_KEY.to_owned(),
                    serde_json::to_value(&prompt.request_id)
                        .unwrap_or(serde_json::Value::Bool(true)),
                );
                let (response_tx, response_rx) = oneshot::channel();
                live.prompt_queue
                    .enqueue(
                        prompt.content,
                        Some(meta),
                        PromptAdmission::Queue,
                        response_tx,
                    )
                    .await;
                let fanout = live.fanout.clone();
                tokio::spawn(async move {
                    if let Ok(Err(error)) = response_rx.await {
                        let _ = fanout.send(SessionEvent::Nori(NoriEvent::RequestFailed(
                            RequestFailure {
                                request_id: None,
                                message: format!(
                                    "The prompt could not be sent on the new route: {error:#}"
                                ),
                                kind: RequestFailureKind::Fatal,
                            },
                        )));
                    }
                });
            }
            // Prompts queued behind it move over, in order, and answer their callers as usual.
            live.prompt_queue.adopt(queued).await;
        }
        Outcome::Cancelled => notice(
            live.fanout,
            &format!("Route change cancelled; staying on {}.", request.from),
        ),
        // Nothing started: the session stays on the agent it has, and the user is told why.
        Outcome::NoRoute(reasons) => {
            let _ = live
                .fanout
                .send(SessionEvent::Nori(NoriEvent::RequestFailed(
                    RequestFailure {
                        request_id: None,
                        message: format!(
                            "{} on {}, and no other route could start ({reasons}).",
                            request.cause.label(),
                            request.from
                        ),
                        kind: RequestFailureKind::Fatal,
                    },
                )));
        }
    }
}

fn notice(fanout: &SessionEventFanout, message: &str) {
    let _ = fanout.send(SessionEvent::Nori(NoriEvent::Notice(
        nori_protocol::Notice {
            message: message.to_owned(),
        },
    )));
}
