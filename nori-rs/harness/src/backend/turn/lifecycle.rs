//! The requests that own a turn (our prompt, our load) and the connection under it.

use tracing::debug;

use super::super::prompt_lifecycle::PromptResult;
use super::super::session_reducer::AgentTurnEnded;
use super::super::session_reducer::CompletedTurn;
use super::super::session_reducer::ReduceOutput;
use super::ledger;
use super::ledger::EndReason;
use crate::normalized::ClientEvent;
use crate::normalized::InterruptReason;
use crate::normalized::PromptCompleted;
use crate::normalized::TurnFailure;
use crate::normalized::WarningInfo;
use crate::normalized::session_runtime::QueuedPromptKind;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::TurnOwner;
use nori_protocol::acp::v1 as acp;

/// End the open turn for `reason` and publish what its owner is owed: a completion for a local
/// request, `LoadCompleted` for a load, an idle announcement for an agent turn. Then start the
/// next queued prompt if the way the turn ended allows it.
pub(in crate::backend) fn terminate(
    runtime: &mut SessionRuntime,
    reason: EndReason,
    out: &mut ReduceOutput,
) {
    let Some(ended) = ledger::end(runtime, reason, out) else {
        return;
    };
    // A summarize-and-swap compaction decides what the conversation is next, so what is queued
    // waits behind a fence until it has swapped or given up, and then starts on the session
    // that results. Whoever finishes the compaction releases the fence.
    let holds_for_compaction = matches!(
        &ended.owner,
        TurnOwner::Local { prompt, .. } if prompt.kind == QueuedPromptKind::Compact
    ) && reason != EndReason::ConnectionLost;
    runtime.fenced |= holds_for_compaction;
    let drain = reason != EndReason::ConnectionLost
        && !holds_for_compaction
        && (ended.start_next_on_end
            || match (&ended.owner, reason) {
                (TurnOwner::Local { .. }, EndReason::Response(acp::StopReason::EndTurn)) => true,
                (
                    TurnOwner::Agent {
                        cancel_sent: false, ..
                    },
                    EndReason::AgentIdle | EndReason::Quiescent,
                ) => true,
                (
                    TurnOwner::Local { .. } | TurnOwner::Agent { .. } | TurnOwner::Replay { .. },
                    _,
                ) => false,
            });
    match &ended.owner {
        TurnOwner::Local {
            request_id, prompt, ..
        } => {
            let (stop_reason, failure) = match reason {
                EndReason::Response(stop_reason) => (stop_reason, None),
                EndReason::Failed(failure) => (acp::StopReason::Cancelled, Some(failure)),
                EndReason::ConnectionLost => (acp::StopReason::Cancelled, Some(TurnFailure::Fatal)),
                EndReason::Loaded
                | EndReason::AgentIdle
                | EndReason::Quiescent
                | EndReason::Abandoned => (acp::StopReason::Cancelled, None),
            };
            out.events
                .push(ClientEvent::PromptCompleted(PromptCompleted {
                    request_id: Some(request_id.clone()),
                    stop_reason,
                    last_agent_message: ended.last_agent_message.clone(),
                    failure,
                }));
            out.completed = Some(CompletedTurn {
                request_id: request_id.clone(),
                prompt: (**prompt).clone(),
                stop_reason,
                last_agent_message: ended.last_agent_message.clone(),
            });
        }
        TurnOwner::Replay { .. } => out.events.push(ClientEvent::LoadCompleted),
        TurnOwner::Agent { cancel_sent, .. } => {
            out.agent_ended = Some(AgentTurnEnded {
                announce_idle: matches!(reason, EndReason::Quiescent | EndReason::Abandoned),
                interrupted: *cancel_sent || reason == EndReason::ConnectionLost,
            });
        }
    }
    ledger::settle(runtime, &ended, drain, out);
}

/// A transport outcome for one wire request. Only the request that owns the current local turn
/// can end it; any other result is late, duplicated or from a superseded prompt, and is inert.
pub(in crate::backend) fn prompt_result(
    runtime: &mut SessionRuntime,
    request_id: &acp::RequestId,
    result: PromptResult,
    out: &mut ReduceOutput,
) {
    let owns_turn = runtime.turn.as_ref().is_some_and(|turn| {
        matches!(&turn.owner, TurnOwner::Local { request_id: owner, .. } if owner == request_id)
    });
    if !owns_turn {
        debug!(target: "acp_event_flow", %request_id, "Ignored a prompt result that owns no turn");
        return;
    }
    let reason = match result {
        PromptResult::Response(stop_reason) => EndReason::Response(stop_reason),
        PromptResult::Failed(failure) => EndReason::Failed(failure),
    };
    terminate(runtime, reason, out);
}

/// The transport assigned the local prompt its wire request ID: it replaces the ID the ledger
/// made up, and clients learn the exact ID that will answer.
pub(in crate::backend) fn prompt_started(
    runtime: &mut SessionRuntime,
    wire_request_id: acp::RequestId,
    out: &mut ReduceOutput,
) {
    let Some(turn) = runtime.turn.as_mut() else {
        debug!(target: "acp_event_flow", %wire_request_id, "Prompt started after its turn ended");
        return;
    };
    let TurnOwner::Local { request_id, .. } = &mut turn.owner else {
        debug!(target: "acp_event_flow", %wire_request_id, "Prompt started in a turn that is not local");
        return;
    };
    *request_id = wire_request_id;
    out.events
        .push(ClientEvent::SessionPhaseChanged(runtime.phase_view()));
}

pub(in crate::backend) fn load_submit(
    runtime: &mut SessionRuntime,
    request_id: acp::RequestId,
    out: &mut ReduceOutput,
) {
    if runtime.turn.is_some() {
        out.events.push(ClientEvent::Warning(WarningInfo {
            message: "Received a load request while a turn is open".to_string(),
        }));
        return;
    }
    ledger::open_request_turn(runtime, TurnOwner::Replay { request_id });
    out.events
        .push(ClientEvent::SessionPhaseChanged(runtime.phase_view()));
}

pub(in crate::backend) fn load_response(
    runtime: &mut SessionRuntime,
    request_id: &acp::RequestId,
    out: &mut ReduceOutput,
) {
    let owns_turn = runtime.turn.as_ref().is_some_and(|turn| {
        matches!(&turn.owner, TurnOwner::Replay { request_id: owner } if owner == request_id)
    });
    if owns_turn {
        terminate(runtime, EndReason::Loaded, out);
    }
}

/// The agent is gone. Whatever turn is open ends as failed, and everything it waited on is
/// answered as cancelled.
pub(in crate::backend) fn connection_lost(runtime: &mut SessionRuntime, out: &mut ReduceOutput) {
    terminate(runtime, EndReason::ConnectionLost, out);
}

/// The attempt a local turn made failed and it will be tried again, possibly elsewhere: whatever
/// that attempt left running is over, and what it said is a message of its own.
pub(in crate::backend) fn attempt_ended(
    runtime: &mut SessionRuntime,
    turn_seq: i64,
    out: &mut ReduceOutput,
) {
    let Some(turn) = runtime
        .turn
        .as_mut()
        .filter(|turn| turn.seq == turn_seq && matches!(turn.owner, TurnOwner::Local { .. }))
    else {
        return;
    };
    turn.work.messages.close();
    ledger::close_unfinished_tools(&mut turn.work.tools, InterruptReason::Failed, out);
}
