//! Passing what a child does on to its parent's stream, and keeping the task's progress.

use std::collections::HashMap;
use std::sync::Arc;

use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

use super::super::Inner;
use super::super::task::TaskEnd;
use super::Active;
use crate::runtime::HarnessHandle;

/// Pass one child event on to the parent. Returns how the active turn ended, when this event
/// ended it.
pub(super) async fn relay(
    orchestrator: &Arc<Inner>,
    harness: &HarnessHandle,
    child_session: &acp::SessionId,
    scope: &acp::SessionId,
    view: Option<&acp::SessionId>,
    active: Option<&mut Active>,
    event: SessionEvent,
) -> Option<TaskEnd> {
    match event {
        SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            mut notification,
        ))) => {
            if let Some(active) = active {
                match &notification.update {
                    acp::SessionUpdate::AgentMessageChunk(acp::ContentChunk {
                        content: acp::ContentBlock::Text(text),
                        ..
                    }) => {
                        active.text.push(&text.text);
                        if let Ok(mut progress) = active.progress.lock()
                            && let Some(line) = active.text.last_line()
                        {
                            progress.activity = line;
                        }
                    }
                    acp::SessionUpdate::ToolCall(call) => {
                        active.text.tool();
                        if !matches!(
                            call.status,
                            acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed
                        ) {
                            active
                                .in_flight_tools
                                .insert(call.tool_call_id.clone(), call.title.clone());
                        }
                        if let Ok(mut progress) = active.progress.lock() {
                            progress.tool_calls += 1;
                            progress.activity = call.title.chars().take(160).collect();
                            note_changes(&mut progress, call.kind, &call.content, &call.locations);
                        }
                    }
                    acp::SessionUpdate::ToolCallUpdate(update) => {
                        active.text.tool();
                        if matches!(
                            update.fields.status,
                            Some(acp::ToolCallStatus::Completed | acp::ToolCallStatus::Failed)
                        ) {
                            active.in_flight_tools.remove(&update.tool_call_id);
                        }
                        if let Ok(mut progress) = active.progress.lock() {
                            note_changes(
                                &mut progress,
                                update.fields.kind.unwrap_or(acp::ToolKind::Other),
                                update.fields.content.as_deref().unwrap_or_default(),
                                update.fields.locations.as_deref().unwrap_or_default(),
                            );
                        }
                    }
                    _ => {}
                }
            }
            let view = view?;
            if let acp::SessionUpdate::ConfigOptionUpdate(update) = &notification.update
                && let Some(model) = super::super::models::current_model(&update.config_options)
            {
                let announce_parent = orchestrator.directory().with(|registry| {
                    let id = registry.by_session(view)?.clone();
                    let actor = registry.actor_mut(&id)?;
                    actor.model = Some(model.clone());
                    Some(actor.announce_parent.clone())
                });
                if let Some(announce_parent) = announce_parent
                    && let Some(parent_session_id) =
                        announce_parent.or_else(|| orchestrator.parent_session())
                {
                    orchestrator.emit(SessionEvent::Nori(NoriEvent::Subagent(
                        SubagentWire::Update(nori_protocol::SubagentUpdate {
                            parent_session_id,
                            session_id: view.clone(),
                            agent: nori_protocol::Patch::Keep,
                            model: nori_protocol::Patch::Set(model),
                            name: nori_protocol::Patch::Keep,
                            title: nori_protocol::Patch::Keep,
                            description: nori_protocol::Patch::Keep,
                            state: nori_protocol::Patch::Keep,
                        }),
                    ))).await;
                }
            }
            notification.session_id = view.clone();
            orchestrator
                .emit(SessionEvent::Acp(AcpEvent::ChildNotification(notification)))
                .await;
            None
        }
        // ACP session IDs are connection-scoped. Distinct child connections may use the same ID.
        SessionEvent::Acp(AcpEvent::ChildNotification(mut notification)) => {
            let original = notification.session_id.clone();
            notification.session_id = scoped_id(scope, &original);
            orchestrator.directory().with(|registry| registry.routes.insert(
                notification.session_id.clone(),
                super::super::DescendantRoute { handle: harness.clone(), session_id: original },
            ));
            orchestrator
                .emit(SessionEvent::Acp(AcpEvent::ChildNotification(notification)))
                .await;
            None
        }
        SessionEvent::Nori(NoriEvent::Subagent(mut wire)) => {
            if let SubagentWire::Update(update) = &wire {
                let public_id = scoped_id(scope, &update.session_id);
                orchestrator.directory().with(|registry| registry.routes.insert(public_id, super::super::DescendantRoute {
                    handle: harness.clone(), session_id: update.session_id.clone(),
                }));
            }
            if let Some(view) = view {
                retarget(&mut wire, child_session, scope, view);
            }
            orchestrator
                .emit(SessionEvent::Nori(NoriEvent::Subagent(wire)))
                .await;
            None
        }
        SessionEvent::Nori(NoriEvent::SubagentQueueChanged { mut session_id, prompts }) => {
            if &session_id == child_session && let Some(view) = view {
                session_id = view.clone();
            } else {
                let original = session_id;
                session_id = scoped_id(scope, &original);
                orchestrator.directory().with(|registry| registry.routes.insert(
                    session_id.clone(),
                    super::super::DescendantRoute { handle: harness.clone(), session_id: original },
                ));
            }
            orchestrator.emit(SessionEvent::Nori(NoriEvent::SubagentQueueChanged { session_id, prompts })).await;
            None
        }
        SessionEvent::Acp(AcpEvent::Request {
            request_id,
            mut request,
        }) => {
            if let acp::AgentRequest::ExtMethodRequest(ext) = &mut request
                && ext.method.as_ref() == "_nori/request_user_input"
                && let Ok(mut params) = serde_json::from_str::<serde_json::Value>(ext.params.get())
                && let Some(id) = params.get("sessionId").and_then(serde_json::Value::as_str)
            {
                let original = acp::SessionId::new(id);
                let public = if &original == child_session {
                    view.cloned()
                } else {
                    let public = scoped_id(scope, &original);
                    orchestrator.directory().with(|registry| registry.routes.insert(public.clone(),
                        super::super::DescendantRoute { handle: harness.clone(), session_id: original }));
                    Some(public)
                };
                if let Some(public) = public {
                    params["sessionId"] = serde_json::Value::String(public.to_string());
                    if let Ok(raw) = serde_json::value::RawValue::from_string(params.to_string()) {
                        ext.params = raw.into();
                    }
                }
            }
            let task = active.map(|active| active.task.clone());
            orchestrator
                .relay_request(harness, task, view.cloned(), request_id, request)
                .await;
            None
        }
        SessionEvent::Acp(AcpEvent::Response {
            request_id,
            response,
        }) => {
            let active = active?;
            if request_id != active.request_id {
                return None;
            }
            Some(match response {
                Ok(acp::AgentResponse::PromptResponse(response)) => {
                    match response.stop_reason {
                        // A cancel Nori asked for has ended the task already; this one the child
                        // chose, and the parent must hear of it.
                        acp::StopReason::Cancelled => TaskEnd::Failed {
                            error: "the child stopped before finishing".to_owned(),
                        },
                        stop_reason => TaskEnd::Completed { stop_reason },
                    }
                }
                Ok(_) => TaskEnd::Completed {
                    stop_reason: acp::StopReason::EndTurn,
                },
                Err(error) => TaskEnd::Failed {
                    error: error.message,
                },
            })
        }
        SessionEvent::Nori(NoriEvent::SessionPhaseChanged(phase)) => {
            let active = active?;
            super::phase::interrupted_turn_ended(
                &mut active.observed_prompt_phase,
                &active.request_id,
                active.interrupted,
                &phase,
            ).then_some(TaskEnd::Cancelled)
        }
        SessionEvent::Nori(NoriEvent::RequestWithdrawn(request_id)) => {
            if let Some(view) = view {
                orchestrator.signal(super::super::Signal::Withdraw(vec![acp::RequestId::Str(format!("{view}/{request_id}"))]));
            }
            None
        }
        SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
            let active = active?;
            (failure.request_id.as_ref() == Some(&active.request_id)).then_some(TaskEnd::Failed {
                error: failure.message,
            })
        }
        SessionEvent::Nori(NoriEvent::SessionEnded(ended)) => {
            active.map(|_| TaskEnd::Failed {
                error: ended
                    .message
                    .unwrap_or_else(|| "the child's session ended".to_owned()),
            })
        }
        // A child that kept working after a cancel, or whose requests were declined, is announced
        // to the user under the child's label, with the ID its permission request was relayed as.
        SessionEvent::Nori(NoriEvent::AgentContinued(mut continued)) => {
            if let Some(view) = view {
                continued.request_id = acp::RequestId::Str(format!("{view}/{}", continued.request_id));
                continued.agent = orchestrator.child_label(view, &continued.agent);
                orchestrator
                    .emit(SessionEvent::Nori(NoriEvent::AgentContinued(continued)))
                    .await;
            }
            None
        }
        SessionEvent::Nori(NoriEvent::RequestDeclined(mut declined)) => {
            if let Some(view) = view {
                let agent = declined.agent.take().unwrap_or_default();
                declined.agent = Some(orchestrator.child_label(view, &agent));
                orchestrator
                    .emit(SessionEvent::Nori(NoriEvent::RequestDeclined(declined)))
                    .await;
            }
            None
        }
        SessionEvent::Nori(NoriEvent::ContextCompacted(compacted)) => {
            if let Some(view) = view {
                let journaled = orchestrator.directory().with(|registry| {
                    let id = registry.by_session(view)?.clone();
                    let actor = registry.actor_mut(&id)?;
                    // Compacting changes what the child remembers, not what the parent sees of
                    // it: identity, alias and state stay; the summary becomes its pinned facts.
                    let digest = super::super::compaction::digest_of(
                        actor,
                        None,
                        compacted.summary.as_deref(),
                        None,
                    );
                    actor.digest = Some(digest);
                    Some(actor.id.clone())
                });
                if let Some(id) = journaled {
                    orchestrator.journal_digest(&id).await;
                }
            }
            None
        }
        SessionEvent::Acp(AcpEvent::Notification(_))
        | SessionEvent::Nori(
            NoriEvent::SessionStarted(_)
            | NoriEvent::SessionReady(_)
            | NoriEvent::QueueChanged(_)
            | NoriEvent::ReplayStarted(_)
            | NoriEvent::ReplayFinished
            | NoriEvent::SessionForked(_)
            | NoriEvent::GoalChanged(_)
            | NoriEvent::CapabilitiesChanged(_)
            | NoriEvent::Undo(_)
            | NoriEvent::UserShell(_)
            | NoriEvent::HookOutput(_)
            | NoriEvent::PromptSummaryUpdated(_)
            | NoriEvent::Notice(_)
            | NoriEvent::PromptUnanswered(_)
            | NoriEvent::QueueRestored(_)
            | NoriEvent::AgentSilent(_)
            | NoriEvent::AgentHeard
            | NoriEvent::SubscriptionExhausted(_)
            | NoriEvent::RouteChanged(_)
            | NoriEvent::RetryScheduled(_)
            | NoriEvent::AttemptSuperseded(_)
            // Results the child's own tasks handed to it are the child's business.
            | NoriEvent::TaskResultsDelivered(_)
            // A child's monitors are its own: they wake it, not its parent.
            | NoriEvent::Monitor(_)
            | NoriEvent::ConversationResponseBoundary(_),
        ) => None,
    }
}

/// The harness moved this child to another route, possibly on another agent: the actor keeps its
/// identity and alias, and its directory entry and the parent's view name the new agent and
/// model. `scope` is the actor's stable identity; `view` is where its activity is shown now.
pub(super) async fn route_changed(
    orchestrator: &Arc<Inner>,
    scope: &acp::SessionId,
    view: Option<&acp::SessionId>,
    changed: &nori_protocol::RouteChanged,
) {
    let display_name = crate::registry::get_agent_display_name(&changed.to.agent);
    let actor = super::super::directory::ActorId::new(scope.clone());
    let announce_parent = orchestrator.directory().with(|registry| {
        let actor = registry.actor_mut(&actor)?;
        actor.agent = changed.to.agent.clone();
        actor.agent_name = display_name;
        actor.model = changed.to.model.clone();
        Some(actor.announce_parent.clone())
    });
    let (Some(view), Some(announce_parent)) = (view, announce_parent) else {
        return;
    };
    if let Some(parent_session_id) = announce_parent.or_else(|| orchestrator.parent_session()) {
        orchestrator
            .emit(SessionEvent::Nori(NoriEvent::Subagent(
                SubagentWire::Update(nori_protocol::SubagentUpdate {
                    parent_session_id,
                    session_id: view.clone(),
                    agent: nori_protocol::Patch::Set(changed.to.agent.clone()),
                    model: changed
                        .to
                        .model
                        .clone()
                        .map_or(nori_protocol::Patch::Keep, nori_protocol::Patch::Set),
                    name: nori_protocol::Patch::Keep,
                    title: nori_protocol::Patch::Keep,
                    description: nori_protocol::Patch::Keep,
                    state: nori_protocol::Patch::Keep,
                }),
            )))
            .await;
    }
}

/// Deterministically close any dangling in-flight tool calls when a child exits, fails, or is cancelled.
pub(super) async fn finish_in_flight_tools(
    orchestrator: &Arc<Inner>,
    view: &acp::SessionId,
    tools: HashMap<acp::ToolCallId, String>,
) {
    for (tool_call_id, _title) in tools {
        let update = acp::ToolCallUpdate::new(
            tool_call_id,
            acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Failed),
        );
        let notification =
            acp::SessionNotification::new(view.clone(), acp::SessionUpdate::ToolCallUpdate(update));
        orchestrator
            .emit(SessionEvent::Acp(AcpEvent::ChildNotification(notification)))
            .await;
    }
}

/// Record the files a tool call changes: those its diffs are of, and the places an edit, delete,
/// or move names.
fn note_changes(
    progress: &mut super::super::task::Progress,
    kind: acp::ToolKind,
    content: &[acp::ToolCallContent],
    locations: &[acp::ToolCallLocation],
) {
    let diffs = content.iter().filter_map(|content| match content {
        acp::ToolCallContent::Diff(diff) => Some(diff.path.display().to_string()),
        _ => None,
    });
    let edited = matches!(
        kind,
        acp::ToolKind::Edit | acp::ToolKind::Delete | acp::ToolKind::Move
    );
    let places = locations
        .iter()
        .filter(|_| edited)
        .map(|location| location.path.display().to_string());
    for path in diffs.chain(places) {
        if !progress.files_changed.contains(&path) {
            progress.files_changed.push(path);
        }
    }
}

/// Re-parent the child's own subagent traffic onto the task's view of the child.
fn retarget(
    wire: &mut SubagentWire,
    child_session: &acp::SessionId,
    scope: &acp::SessionId,
    view: &acp::SessionId,
) {
    let swap = |id: &mut acp::SessionId| {
        if id == child_session {
            *id = view.clone();
        } else {
            *id = scoped_id(scope, id);
        }
    };
    match wire {
        SubagentWire::Update(update) => {
            swap(&mut update.parent_session_id);
            swap(&mut update.session_id);
        }
        SubagentWire::Message(message) => {
            swap(&mut message.session_id);
            if let Some(sender) = message.sender_session_id.as_mut() {
                swap(sender);
            }
            if let Some(recipient) = message.recipient_session_id.as_mut() {
                swap(recipient);
            }
        }
    }
}

/// Stable, unambiguous public identity for an ID on this child connection.
pub(in crate::backend::orchestrator) fn scoped_id(
    scope: &acp::SessionId,
    id: &acp::SessionId,
) -> acp::SessionId {
    let scope = scope.to_string();
    acp::SessionId::new(format!("nori-relay:{}:{scope}{id}", scope.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nori_protocol::Patch;
    use nori_protocol::SubagentUpdate;
    use pretty_assertions::assert_eq;

    #[test]
    fn identical_connection_local_ids_remain_distinct_and_keep_ancestry() {
        let raw = acp::SessionId::new("native-child");
        let first = acp::SessionId::new("first-parent");
        let second = acp::SessionId::new("second-parent");
        assert_ne!(scoped_id(&first, &raw), scoped_id(&second, &raw));
        assert_eq!(scoped_id(&first, &raw), scoped_id(&first, &raw));
        assert_ne!(
            scoped_id(&acp::SessionId::new("a"), &acp::SessionId::new("bc")),
            scoped_id(&acp::SessionId::new("ab"), &acp::SessionId::new("c"))
        );
        let mut wire = SubagentWire::Update(SubagentUpdate {
            parent_session_id: acp::SessionId::new("provider-root"),
            session_id: raw.clone(),
            agent: Patch::Keep,
            model: Patch::Keep,
            title: Patch::Keep,
            description: Patch::Keep,
            state: Patch::Keep,
        });
        retarget(
            &mut wire,
            &acp::SessionId::new("provider-root"),
            &first,
            &first,
        );
        let SubagentWire::Update(update) = wire else {
            panic!("expected update")
        };
        assert_eq!(update.parent_session_id, first.clone());
        assert_eq!(update.session_id, scoped_id(&first, &raw));
    }
}
