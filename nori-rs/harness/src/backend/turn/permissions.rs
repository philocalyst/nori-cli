//! Requests the agent waits on. They are valid in every turn: the ledger tracks them in the turn
//! that owns them and they are answered by approval policy or the user exactly as in a prompt.
//! A request is answered as cancelled only when the user cancels its turn and the agent has not
//! yet ignored that, or the connection dies. Nothing else may answer one on the user's behalf.
//! When the agent kept working after a cancel it ignored, or after its turn ended, the request
//! goes to the user with a typed announcement of what happened.

use nori_protocol::AgentTurnCause;

use std::time::Duration;

use super::super::session_reducer::ReduceOutput;
use super::super::session_reducer::SideEffect;
use super::super::session_reducer::Wake;
use super::ledger;
use super::quiescence;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::TurnOwner;

/// How long identical declines are folded into one notice.
const DECLINE_WINDOW: Duration = Duration::from_secs(3);

pub(in crate::backend) fn opened(
    runtime: &mut SessionRuntime,
    request_id: String,
    call_id: Option<String>,
    title: &str,
    out: &mut ReduceOutput,
) {
    let turn = ledger::agent_turn(runtime, AgentTurnCause::Unprompted, out);
    turn.work.messages.close();
    let cancel_ignored = turn.work.cancel_ignored;
    // The user cancelled and the agent has not had its chance to stop: the request is declined,
    // once, with a notice. If the agent ignored the cancel the user decides instead.
    if turn.is_cancelling() && !cancel_ignored {
        decline(runtime, request_id, title, out);
        return;
    }
    if let Some(call_id) = &call_id {
        if let TurnOwner::Agent { cause, .. } = &turn.owner
            && (cancel_ignored || *cause == AgentTurnCause::Detached)
        {
            out.side_effects.push(SideEffect::AnnounceContinued {
                request_id: request_id.clone(),
                tool: title.to_owned(),
                cause: *cause,
                after_cancel: cancel_ignored,
            });
        }
        turn.work.track_tool(call_id, None, false);
    }
    turn.work.pending_requests.insert(request_id);
    quiescence::touch(turn, out);
}

/// Answer a request as cancelled and say so. Declines of the same tool inside the window are
/// folded into one notice that carries their count, so an agent that keeps asking cannot turn
/// the notice into a stream.
fn decline(runtime: &mut SessionRuntime, request_id: String, title: &str, out: &mut ReduceOutput) {
    out.side_effects
        .push(SideEffect::ResolvePermissionCancelled { request_id });
    let count = runtime.declines.entry(title.to_owned()).or_insert(0);
    *count += 1;
    if *count == 1 {
        out.side_effects.push(SideEffect::AnnounceDeclined {
            tool: title.to_owned(),
            count: 1,
        });
        out.side_effects.push(SideEffect::Schedule {
            wake: Wake::DeclineSummary {
                tool: title.to_owned(),
            },
            after: DECLINE_WINDOW,
        });
    }
}

/// The window for `tool` is over: if more than the noticed request was declined in it, say how
/// many.
pub(in crate::backend) fn decline_window_over(
    runtime: &mut SessionRuntime,
    tool: &str,
    out: &mut ReduceOutput,
) {
    if let Some(count) = runtime.declines.remove(tool)
        && count > 1
    {
        out.side_effects.push(SideEffect::AnnounceDeclined {
            tool: tool.to_owned(),
            count,
        });
    }
}

pub(in crate::backend) fn answered(
    runtime: &mut SessionRuntime,
    request_id: &str,
    out: &mut ReduceOutput,
) {
    let Some(turn) = runtime.turn.as_mut() else {
        return;
    };
    if turn.work.pending_requests.remove(request_id) {
        quiescence::touch(turn, out);
    }
}
