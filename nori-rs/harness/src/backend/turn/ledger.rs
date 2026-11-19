//! Opening and ending turns.

use std::collections::BTreeSet;
use std::time::Duration;

use nori_protocol::AgentTurnCause;

use super::super::session_reducer::ReduceOutput;
use super::super::session_reducer::SideEffect;
use super::super::session_reducer::Wake;
use crate::normalized::ClientEvent;
use crate::normalized::InterruptReason;
use crate::normalized::ToolsInterrupted;
use crate::normalized::TurnFailure;
use crate::normalized::session_runtime::SessionPhaseView;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::Detachment;
use crate::normalized::turn::TrackedTool;
use crate::normalized::turn::Turn;
use crate::normalized::turn::TurnOwner;
use crate::normalized::turn::TurnWork;
use nori_protocol::acp::v1 as acp;

/// How long after a turn ends the agent's continued activity still counts as that turn's.
const DETACH_WINDOW: Duration = Duration::from_secs(10);

/// Why a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::backend) enum EndReason {
    /// The prompt response arrived.
    Response(acp::StopReason),
    /// The prompt failed, or Nori ended it.
    Failed(TurnFailure),
    /// The load response arrived.
    Loaded,
    /// The agent reported `idle`.
    AgentIdle,
    /// The agent went quiet.
    Quiescent,
    /// The user cancelled an agent turn and it did not stop; Nori stopped tracking it.
    Abandoned,
    /// The connection to the agent died.
    ConnectionLost,
}

/// What is left of an ended turn once the ledger has closed it.
pub(in crate::backend) struct EndedTurn {
    pub owner: TurnOwner,
    pub last_agent_message: Option<String>,
    pub start_next_on_end: bool,
    /// Requests the agent is still waiting on that the ended turn passed to its successor.
    pub carried_requests: BTreeSet<String>,
}

/// The turn that accounts for agent activity: the open one, or a new `Agent` turn. `cause` is
/// the cause when nothing ties the activity to a turn that just ended.
pub(in crate::backend) fn agent_turn<'a>(
    runtime: &'a mut SessionRuntime,
    cause: AgentTurnCause,
    out: &mut ReduceOutput,
) -> &'a mut Turn {
    let mut opened = false;
    let turn = runtime.turn.get_or_insert_with(|| {
        opened = true;
        let detachment = runtime.detached.take();
        let seq = runtime.next_turn_seq;
        runtime.next_turn_seq += 1;
        let cause = match (&detachment, cause) {
            (Some(_), AgentTurnCause::Unprompted) => AgentTurnCause::Detached,
            (_, cause) => cause,
        };
        let cancelled = detachment.is_some_and(|detachment| detachment.cancelled);
        Turn {
            seq,
            owner: TurnOwner::Agent {
                cause,
                cancel_sent: cancelled,
            },
            work: TurnWork {
                cancel_ignored: cancelled,
                ..TurnWork::default()
            },
        }
    });
    if opened {
        out.events
            .push(ClientEvent::SessionPhaseChanged(SessionPhaseView::Agent));
    }
    turn
}

/// Open the turn for a request of ours. The session must have no turn.
pub(in crate::backend) fn open_request_turn(runtime: &mut SessionRuntime, owner: TurnOwner) {
    let seq = runtime.next_turn_seq;
    runtime.next_turn_seq += 1;
    runtime.detached = None;
    runtime.turn = Some(Turn {
        seq,
        owner,
        work: TurnWork::default(),
    });
}

/// End the current turn. Closes its open message, closes every tool call it left unfinished,
/// and publishes the idle phase. The caller publishes whatever completion the owner has and then
/// calls [`settle`].
pub(in crate::backend) fn end(
    runtime: &mut SessionRuntime,
    reason: EndReason,
    out: &mut ReduceOutput,
) -> Option<EndedTurn> {
    let mut turn = runtime.turn.take()?;
    let last_agent_message = turn.work.messages.finish();

    let interrupt = match reason {
        EndReason::Response(acp::StopReason::Cancelled) => InterruptReason::Cancelled,
        EndReason::Response(_)
        | EndReason::Loaded
        | EndReason::AgentIdle
        | EndReason::Quiescent => InterruptReason::TurnEnded,
        EndReason::Failed(TurnFailure::CancelIgnored) | EndReason::Abandoned => {
            InterruptReason::Cancelled
        }
        EndReason::Failed(TurnFailure::Retryable | TurnFailure::Fatal) => InterruptReason::Failed,
        EndReason::ConnectionLost => InterruptReason::ConnectionLost,
    };
    close_unfinished_tools(&mut turn.work.tools, interrupt, out);

    // Requests the agent still waits on stay answerable. A local request that ended with some
    // outstanding hands them to the agent turn that continues it; a dead connection or a user
    // cancellation answers them as cancelled, the only two things that may.
    let pending = std::mem::take(&mut turn.work.pending_requests);
    let carried_requests = if reason == EndReason::ConnectionLost || turn.is_cancelling() {
        out.side_effects.extend(
            pending
                .into_iter()
                .map(|request_id| SideEffect::ResolvePermissionCancelled { request_id }),
        );
        BTreeSet::new()
    } else if matches!(
        turn.owner,
        TurnOwner::Local { .. } | TurnOwner::Replay { .. }
    ) {
        pending
    } else {
        BTreeSet::new()
    };

    runtime.detached = match &turn.owner {
        TurnOwner::Local { .. } => Some(Detachment {
            turn_seq: turn.seq,
            cancelled: reason == EndReason::Failed(TurnFailure::CancelIgnored),
        }),
        TurnOwner::Agent { .. } if reason == EndReason::Abandoned => Some(Detachment {
            turn_seq: turn.seq,
            cancelled: true,
        }),
        TurnOwner::Agent { .. } | TurnOwner::Replay { .. } => None,
    };
    if runtime.detached.is_some() {
        out.side_effects.push(SideEffect::Schedule {
            wake: Wake::DetachmentExpiry { turn_seq: turn.seq },
            after: DETACH_WINDOW,
        });
    }

    out.events
        .push(ClientEvent::SessionPhaseChanged(SessionPhaseView::Idle));
    Some(EndedTurn {
        owner: turn.owner,
        last_agent_message,
        start_next_on_end: turn.work.start_next_on_end,
        carried_requests,
    })
}

/// Finish ending a turn: continue its outstanding requests as an agent turn, then start the next
/// queued prompt when `drain` says so and nothing is open. The steering intent of the ended turn
/// moves to the turn that continues it. Requests a load left outstanding continue as an
/// unprompted agent turn; those of a local request as a detached one.
pub(in crate::backend) fn settle(
    runtime: &mut SessionRuntime,
    ended: &EndedTurn,
    drain: bool,
    out: &mut ReduceOutput,
) {
    if !ended.carried_requests.is_empty() {
        let cause = match ended.owner {
            TurnOwner::Replay { .. } => AgentTurnCause::Unprompted,
            TurnOwner::Local { .. } | TurnOwner::Agent { .. } => AgentTurnCause::Detached,
        };
        let turn = agent_turn(runtime, cause, out);
        turn.work.pending_requests = ended.carried_requests.clone();
        turn.work.start_next_on_end = ended.start_next_on_end;
    }
    if drain {
        super::queue::start_next_queued(runtime, out);
    }
}

/// Mark every tool call the turn left unfinished as failed and publish that, so nothing keeps
/// showing as running.
pub(in crate::backend) fn close_unfinished_tools(
    tools: &mut [TrackedTool],
    reason: InterruptReason,
    out: &mut ReduceOutput,
) {
    let call_ids = tools
        .iter_mut()
        .filter(|tool| !tool.is_terminal())
        .map(|tool| {
            tool.status = acp::ToolCallStatus::Failed;
            tool.call_id.clone()
        })
        .collect::<Vec<_>>();
    if !call_ids.is_empty() {
        out.events
            .push(ClientEvent::ToolsInterrupted(ToolsInterrupted {
                call_ids,
                reason,
            }));
    }
}
