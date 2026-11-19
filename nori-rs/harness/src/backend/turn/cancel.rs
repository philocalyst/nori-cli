//! What cancelling does to each kind of turn.

use std::time::Duration;

use tracing::debug;

use super::super::session_reducer::CancelTarget;
use super::super::session_reducer::ReduceOutput;
use super::super::session_reducer::SideEffect;
use super::super::session_reducer::Wake;
use super::ledger;
use crate::normalized::ClientEvent;
use crate::normalized::InterruptReason;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::TurnOwner;
use nori_protocol::acp::v1 as acp;

/// How long an agent turn may ignore `session/cancel` before Nori stops tracking it.
const AGENT_CANCEL_DEADLINE: Duration = Duration::from_secs(10);

/// Cancel the open turn: answer what it waits on as cancelled, close its unfinished tool calls,
/// and send `session/cancel`. Cancelling again sends `session/cancel` again and nothing else,
/// which is how the user presses through an agent that ignored the first one. A request-scoped
/// cancel only reaches the local turn that request owns.
pub(in crate::backend) fn cancel(
    runtime: &mut SessionRuntime,
    only_request: Option<&acp::RequestId>,
    out: &mut ReduceOutput,
) {
    let Some(turn) = runtime.turn.as_mut() else {
        return;
    };
    let seq = turn.seq;
    let (target, first) = match &mut turn.owner {
        TurnOwner::Local {
            request_id,
            cancelling,
            ..
        } => {
            if only_request.is_some_and(|only| *only != *request_id) {
                return;
            }
            let first = !std::mem::replace(cancelling, true);
            (CancelTarget::Request(request_id.clone()), first)
        }
        TurnOwner::Agent { cancel_sent, .. } => {
            if only_request.is_some() {
                return;
            }
            let first = !std::mem::replace(cancel_sent, true);
            (CancelTarget::AgentTurn(seq), first)
        }
        TurnOwner::Replay { .. } => return,
    };

    // Whenever the user cancels, whatever the agent still waits on is answered as cancelled. That
    // includes a repeat: pressing through an agent that ignored the first cancel is the user
    // declining what it asked in the meantime.
    out.side_effects.extend(
        std::mem::take(&mut turn.work.pending_requests)
            .into_iter()
            .map(|request_id| SideEffect::ResolvePermissionCancelled { request_id }),
    );
    if first {
        ledger::close_unfinished_tools(&mut turn.work.tools, InterruptReason::Cancelled, out);
        out.events
            .push(ClientEvent::SessionPhaseChanged(runtime.phase_view()));
    }
    // Every cancel of an agent turn starts a fresh deadline, including the user's repeats: that
    // is what lets the user press through an agent that keeps ignoring the cancel.
    if matches!(target, CancelTarget::AgentTurn(_)) {
        out.side_effects.push(SideEffect::Schedule {
            wake: Wake::CancelDeadline { turn_seq: seq },
            after: AGENT_CANCEL_DEADLINE,
        });
    }
    debug!(target: "acp_event_flow", ?target, first, "Reducer cancelled the open turn");
    out.side_effects.push(SideEffect::SendCancel {
        target,
        arm_watchdog: first,
    });
}

/// An agent turn the user cancelled did not stop in time: stop tracking it. If the agent is
/// still working its next activity opens a new turn that remembers the cancel.
pub(in crate::backend) fn deadline(
    runtime: &mut SessionRuntime,
    turn_seq: i64,
    out: &mut ReduceOutput,
) {
    let cancelled_agent_turn = runtime.turn.as_ref().is_some_and(|turn| {
        turn.seq == turn_seq
            && matches!(
                turn.owner,
                TurnOwner::Agent {
                    cancel_sent: true,
                    ..
                }
            )
    });
    if !cancelled_agent_turn {
        return;
    }
    out.events
        .push(ClientEvent::Warning(crate::normalized::WarningInfo {
            message: format!(
                "The agent kept working {}s after cancel was sent, so Nori stopped waiting for it. \
             If it is still running it will show as running again; press Esc to send cancel again.",
                AGENT_CANCEL_DEADLINE.as_secs()
            ),
        }));
    super::lifecycle::terminate(runtime, ledger::EndReason::Abandoned, out);
}
