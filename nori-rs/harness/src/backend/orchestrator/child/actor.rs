//! A child's actor: the one task that owns the child's event stream. It runs the turns it is
//! handed one at a time, relays what the child does into the owner's stream, and reports each
//! turn's end with the child's final message. When the child's session ends it reports the actor
//! gone.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc;

use super::Command;
use super::FinalText;
use super::Turn;
use super::relay;
use crate::backend::orchestrator::Inner;
use crate::backend::orchestrator::directory::ActorId;
use crate::backend::orchestrator::task::TaskEnd;
use crate::backend::orchestrator::task::TaskId;
use crate::runtime::HarnessHandle;

/// A child whose own delegations all ended is given this long to start the turn that takes their
/// results before its task ends without one.
const CHILDREN_SETTLE: Duration = Duration::from_secs(3);

pub(super) struct Active {
    pub(super) task: TaskId,
    pub(super) view_id: acp::SessionId,
    pub(super) request_id: acp::RequestId,
    pub(super) text: FinalText,
    pub(super) progress: crate::backend::orchestrator::task::SharedProgress,
    /// In-flight tool calls observed during this turn, completed deterministically if aborted.
    pub(super) in_flight_tools: HashMap<acp::ToolCallId, String>,
    /// The turn ended while the child's own delegations ran: the task goes on until they end
    /// and the turn that takes their results ends too.
    pub(super) awaiting_children: bool,
    pub(super) interrupted: bool,
    pub(super) observed_prompt_phase: bool,
    /// Where the turn ran when that was not where it was asked to: recovery moved the child to
    /// another route. The parent reads this with the result.
    pub(super) route_notes: Vec<String>,
}

pub(super) async fn run_actor(
    inner: Weak<Inner>,
    harness: HarnessHandle,
    mut child_session: acp::SessionId,
    mut events: mpsc::UnboundedReceiver<SessionEvent>,
    mut turns: mpsc::UnboundedReceiver<Command>,
    staged: bool,
    scope: acp::SessionId,
) {
    let mut active: Option<Active> = None;
    let mut pending: VecDeque<Turn> = VecDeque::new();
    let mut views = HashMap::new();
    let mut channel_open = true;
    // Activity that arrives between tasks is shown under the task that caused it.
    let mut last_view: Option<acp::SessionId> = (!staged).then(|| scope.clone());
    // The child's own delegated tasks still running, by their view ids.
    let mut delegations: HashSet<acp::SessionId> = HashSet::new();
    let mut settle_by: Option<tokio::time::Instant> = None;
    // The harness replaced the child's agent: the prompt it sends again on the new agent has a
    // new request ID, which the active turn must follow.
    let mut follow_resent_prompt = false;
    // Staging must not leak notifications or permission requests to the parent before commit.
    // Keep events in order, while noticing a dead provider so an attach cannot publish it.
    let mut buffered = VecDeque::new();
    let mut staging = staged;
    let mut staged_view = None;
    let mut session_dead = false;
    // Why the session ended, when it ended on its own.
    let mut exit: Option<String> = None;
    while staging {
        tokio::select! {
            command = turns.recv() => match command {
                Some(Command::Attach(view)) => {
                    staged_view = Some(view);
                }
                Some(Command::Activate) => {
                    if let Some(view) = staged_view.take() {
                        if let Some(orchestrator) = inner.upgrade()
                            && let Some(parent) = orchestrator.parent_for_view(&view)
                        {
                            views.entry(view.clone()).or_insert(parent);
                        }
                        last_view = Some(view);
                        staging = false;
                    }
                }
                Some(Command::Run(turn)) => pending.push_back(turn),
                Some(Command::Cancel(tx)) => {
                    let _ = tx.send(Err("the child is not yet adopted".to_owned()));
                }
                None => { session_dead = true; break; }
            },
            event = events.recv() => match event {
                Some(SessionEvent::Nori(NoriEvent::SessionEnded(ended))) => {
                    exit = ended.message;
                    session_dead = true;
                    break;
                }
                None => {
                    session_dead = true;
                    break;
                }
                Some(event) => buffered.push_back(event),
            },
        }
    }
    if !session_dead {
        loop {
            tokio::select! {
                command = turns.recv(), if channel_open => {
                    match command {
                        Some(Command::Activate) => {}
                        Some(Command::Attach(view)) => {
                            if let Some(orchestrator) = inner.upgrade()
                                && let Some(parent) = orchestrator.parent_for_view(&view)
                            {
                                views.entry(view.clone()).or_insert(parent);
                            }
                            last_view = Some(view);
                        }
                        Some(Command::Run(turn)) => {
                            let view = turn.view_id.clone();
                            if let Some(orchestrator) = inner.upgrade()
                                && let Some(parent) = orchestrator.parent_for_view(&view)
                            {
                                views.entry(view).or_insert(parent);
                            }
                            pending.push_back(turn);
                        }
                        Some(Command::Cancel(tx)) => {
                            let result = if let Some(active) = active.as_mut() {
                                let result = harness.cancel().await.map_err(|error| error.to_string());
                                if result.is_ok() { active.interrupted = true; }
                                result
                            } else {
                                Ok(())
                            };
                            let _ = tx.send(result);
                        }
                        None => channel_open = false,
                    }
                }
                () = std::future::ready(()), if active.is_none() && !pending.is_empty() => {
                    let Some(turn) = pending.pop_front() else { continue };
                    let Some(orchestrator) = inner.upgrade() else { break };
                    if orchestrator.task_has_ended(&turn.task).await {
                        // Cancelled while it waited for the child.
                        continue;
                    }
                    last_view = Some(turn.view_id.clone());
                    let content = turn.content.unwrap_or_else(|| vec![acp::ContentBlock::Text(acp::TextContent::new(turn.prompt))]);
                    match harness.prompt(content).await {
                        Ok(request_id) => {
                            orchestrator.task_running(&turn.task).await;
                            active = Some(Active {
                                task: turn.task,
                                view_id: turn.view_id,
                                request_id,
                                text: FinalText::default(),
                                progress: turn.progress,
                                in_flight_tools: HashMap::new(),
                                awaiting_children: false,
                                interrupted: false,
                                observed_prompt_phase: false,
                                route_notes: Vec::new(),
                            });
                        }
                        Err(error) => {
                            orchestrator
                                .task_ended(&turn.task, TaskEnd::Failed { error: format!("{error:#}") }, String::new())
                                .await;
                        }
                    }
                }
                () = sleep_until(settle_by), if settle_by.is_some() => {
                    settle_by = None;
                    let Some(orchestrator) = inner.upgrade() else { break };
                    if let Some(ended) = active.take() {
                        let end = if ended.interrupted { TaskEnd::Cancelled } else { completed() };
                        finish_active_turn(&orchestrator, ended, end).await;
                    }
                }
                event = async {
                    match buffered.pop_front() {
                        Some(event) => Some(event),
                        None => events.recv().await,
                    }
                } => {
                    let Some(orchestrator) = inner.upgrade() else { break };
                    let Some(event) = event else {
                        exit = Some("the child's agent exited".to_owned());
                        if let Some(ended) = active.take() {
                            finish_active_turn(
                                &orchestrator,
                                ended,
                                TaskEnd::Failed { error: "the child's agent exited".to_owned() },
                            ).await;
                        }
                        break;
                    };
                    // Rewind and compact replace the child's provider session. Its orchestrator
                    // immediately uses that new id for child updates, so refresh before tracking or
                    // retargeting those updates to the stable view id.
                    if matches!(&event, SessionEvent::Nori(NoriEvent::Subagent(_)))
                        && let Ok(current) = harness.current_session_id().await
                    {
                        if current != child_session {
                            let actor = ActorId::new(scope.clone());
                            orchestrator.directory().with(|registry| registry.note_session(&actor, current.clone()));
                        }
                        child_session = current;
                    }
                    // A turn starting while the task waits on the child's delegations is the one that
                    // takes their results.
                    if let SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Prompting { request_id })) = &event
                        && let Some(waiting) = active.as_mut().filter(|active| active.awaiting_children)
                    {
                        waiting.request_id = request_id.clone();
                        waiting.awaiting_children = false;
                        settle_by = None;
                    }
                    if let SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) = &event
                        && update.parent_session_id == child_session
                        && let nori_protocol::Patch::Set(state) = &update.state
                    {
                        match state {
                            SubagentState::Running => {
                                delegations.insert(update.session_id.clone());
                            }
                            SubagentState::Idle | SubagentState::Ended(_) => {
                                delegations.remove(&update.session_id);
                                if delegations.is_empty()
                                    && active.as_ref().is_some_and(|active| active.awaiting_children)
                                {
                                    settle_by = Some(tokio::time::Instant::now() + CHILDREN_SETTLE);
                                }
                            }
                        }
                    }
                    let view = active.as_ref().map(|active| active.view_id.clone()).or_else(|| last_view.clone());
                    // A route change keeps the child's identity and alias; if its agent was
                    // replaced, map the new session id to the same actor and follow its prompt.
                    if let SessionEvent::Nori(NoriEvent::RouteChanged(changed)) = &event {
                        if let Ok(current) = harness.current_session_id().await {
                            if current != child_session {
                                let actor = ActorId::new(scope.clone());
                                orchestrator.directory().with(|registry| registry.note_session(&actor, current.clone()));
                                follow_resent_prompt = active.is_some();
                            }
                            child_session = current;
                        }
                        if let Some(turn) = active.as_mut() {
                            turn.route_notes.push(format!(
                                "ran on {} after {} on {}",
                                changed.to,
                                changed.cause.label(),
                                changed.from
                            ));
                        }
                        orchestrator.seed.tree.program.note(
                            &scope.to_string(),
                            super::super::RecoveryNote::Route(changed.clone()),
                        );
                        relay::route_changed(&orchestrator, &scope, view.as_ref(), changed).await;
                    }
                    if let SessionEvent::Nori(NoriEvent::RetryScheduled(retry)) = &event {
                        orchestrator.seed.tree.program.note(
                            &scope.to_string(),
                            super::super::RecoveryNote::Retry(retry.clone()),
                        );
                    }
                    if follow_resent_prompt
                        && let SessionEvent::Nori(NoriEvent::SessionPhaseChanged(SessionPhase::Prompting { request_id })) = &event
                        && let Some(turn) = active.as_mut()
                    {
                        turn.request_id = request_id.clone();
                        turn.observed_prompt_phase = false;
                        follow_resent_prompt = false;
                    }
                    let session_ended = matches!(&event, SessionEvent::Nori(NoriEvent::SessionEnded(_)));
                    if let SessionEvent::Nori(NoriEvent::SessionEnded(ended)) = &event {
                        exit = Some(ended.message.clone().unwrap_or_else(|| "the child's session ended".to_owned()));
                    }
                    let end = relay::relay(&orchestrator, &harness, &child_session, &scope, view.as_ref(), active.as_mut(), event).await;
                    if session_ended {
                        if let (Some(end), Some(ended)) = (end, active.take()) {
                            finish_active_turn(&orchestrator, ended, end).await;
                        }
                        break;
                    }
                    let Some(end) = end else {
                        continue;
                    };
                    if matches!(end, TaskEnd::Completed { .. }) && !delegations.is_empty() {
                        if let Some(waiting) = active.as_mut() {
                            waiting.awaiting_children = true;
                        }
                        continue;
                    }
                    if let Some(ended) = active.take() {
                        let end = if ended.interrupted { TaskEnd::Cancelled } else { end };
                        finish_active_turn(&orchestrator, ended, end).await;
                    }
                }
            }
            if !channel_open && pending.is_empty() && active.is_none() {
                break;
            }
        }
    }
    // Turns handed over as the actor stopped would otherwise wait forever.
    turns.close();
    if let Some(orchestrator) = inner.upgrade() {
        while let Ok(command) = turns.try_recv() {
            match command {
                Command::Attach(view) => {
                    if staging {
                        staged_view = Some(view);
                    } else if let Some(parent) = orchestrator.parent_for_view(&view) {
                        views.entry(view).or_insert(parent);
                    }
                }
                Command::Activate => {
                    if let Some(view) = staged_view.take()
                        && let Some(parent) = orchestrator.parent_for_view(&view)
                    {
                        views.entry(view).or_insert(parent);
                    }
                }
                Command::Run(turn) => pending.push_back(turn),
                Command::Cancel(tx) => {
                    let _ = tx.send(Err("the child session ended".to_owned()));
                }
            }
        }
    }
    let _ = harness.shutdown().await;
    if let Some(orchestrator) = inner.upgrade() {
        // Whatever the actor was handed and never ran fails with it, and the actor stays in the
        // directory, ended, until a message revives it.
        orchestrator
            .actor_gone(
                &ActorId::new(scope.clone()),
                match exit {
                    Some(reason) => SubagentOutcome::Failed {
                        reason: Some(reason),
                    },
                    None => SubagentOutcome::Closed,
                },
            )
            .await;
        for (session_id, parent_session_id) in views {
            orchestrator
                .emit(SessionEvent::Nori(NoriEvent::Subagent(
                    SubagentWire::Update(nori_protocol::SubagentUpdate {
                        parent_session_id,
                        session_id,
                        agent: nori_protocol::Patch::Keep,
                        model: nori_protocol::Patch::Keep,
                        title: nori_protocol::Patch::Keep,
                        description: nori_protocol::Patch::Keep,
                        state: nori_protocol::Patch::Set(SubagentState::Ended(
                            nori_protocol::SubagentOutcome::Closed,
                        )),
                    }),
                )))
                .await;
        }
    }
}

async fn finish_active_turn(orchestrator: &Arc<Inner>, mut ended: Active, end: TaskEnd) {
    if !matches!(end, TaskEnd::Completed { .. }) && !ended.in_flight_tools.is_empty() {
        let tools = std::mem::take(&mut ended.in_flight_tools);
        relay::finish_in_flight_tools(orchestrator, &ended.view_id, tools).await;
    }
    if ended.interrupted {
        orchestrator.user_interrupted(&ended.task).await;
    }
    let mut text = ended.text.finish();
    let tool_calls = ended
        .progress
        .lock()
        .map_or(1, |progress| progress.tool_calls);
    // A turn that ends with no words and no tool calls did nothing: never report it as success to
    // someone waiting for a result.
    let end = if matches!(
        end,
        TaskEnd::Completed {
            stop_reason: acp::StopReason::EndTurn
        }
    ) && text.trim().is_empty()
        && tool_calls == 0
        && orchestrator.expects_result(&ended.task)
    {
        TaskEnd::Failed {
            error: crate::backend::orchestrator::text::NO_OUTPUT.to_owned(),
        }
    } else {
        end
    };
    // A failure recovery handled is not the task's failure, but where it ran is the parent's to
    // know: the model that reads the result must not take it for the model it asked for.
    if !ended.route_notes.is_empty() {
        text = format!("[Route: {}.]\n{text}", ended.route_notes.join("; "));
    }
    orchestrator.task_ended(&ended.task, end, text).await;
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

fn completed() -> TaskEnd {
    TaskEnd::Completed {
        stop_reason: acp::StopReason::EndTurn,
    }
}
