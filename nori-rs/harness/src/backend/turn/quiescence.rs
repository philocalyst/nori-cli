//! When an agent turn is over.
//!
//! An agent turn has no response to end it. It ends when the agent reports `idle`, or when it is
//! quiescent: every tool call it started is terminal, nothing it asked is unanswered, and it has
//! made no sound for a quiet window. The window is a timer the driver runs for the reducer. New
//! activity does not cancel it; it makes the wake that comes back stale, and the reducer re-arms.

use std::collections::BTreeSet;
use std::time::Duration;

use nori_protocol::AgentTurnCause;

use super::super::session_reducer::ReduceOutput;
use super::super::session_reducer::SideEffect;
use super::super::session_reducer::Wake;
use super::cancel;
use super::ledger::EndReason;
use super::lifecycle;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::Turn;
use crate::normalized::turn::TurnOwner;

/// Silence that ends an agent turn nobody declared.
const QUIET_WINDOW: Duration = Duration::from_secs(3);
/// Silence that ends a turn the agent declared with a `working` status. It said it would send
/// `idle`, so only a long silence is evidence it forgot.
const DECLARED_QUIET_WINDOW: Duration = Duration::from_secs(30);

/// Record activity in `turn`. An agent turn arms its quiet-window timer if none is running.
pub(in crate::backend) fn touch(turn: &mut Turn, out: &mut ReduceOutput) {
    turn.work.activity += 1;
    if std::mem::take(&mut turn.work.silent_published) {
        out.side_effects.push(SideEffect::AnnounceHeard);
    }
    let TurnOwner::Agent { cause, .. } = &turn.owner else {
        return;
    };
    if turn.work.quiet_timer_armed {
        return;
    }
    turn.work.quiet_timer_armed = true;
    out.side_effects.push(SideEffect::Schedule {
        wake: Wake::QuietWindow {
            turn_seq: turn.seq,
            activity: turn.work.activity,
        },
        after: match cause {
            AgentTurnCause::Observed => DECLARED_QUIET_WINDOW,
            AgentTurnCause::Detached | AgentTurnCause::Unprompted => QUIET_WINDOW,
        },
    });
}

pub(in crate::backend) fn woke(
    runtime: &mut SessionRuntime,
    wake: &Wake,
    live_requests: &BTreeSet<String>,
    out: &mut ReduceOutput,
) {
    match wake {
        Wake::QuietWindow { turn_seq, activity } => {
            let Some(turn) = runtime.turn.as_mut().filter(|turn| turn.seq == *turn_seq) else {
                return;
            };
            turn.work.quiet_timer_armed = false;
            turn.work
                .pending_requests
                .retain(|id| live_requests.contains(id));
            if turn.work.activity != *activity {
                // Something happened during the window: look again after another one.
                touch(turn, out);
                return;
            }
            if turn.work.is_quiescent() {
                lifecycle::terminate(runtime, EndReason::Quiescent, out);
            }
        }
        Wake::CancelDeadline { turn_seq } => cancel::deadline(runtime, *turn_seq, out),
        Wake::SilenceTick { turn_seq } => silence_tick(runtime, *turn_seq, out),
        Wake::DeclineSummary { tool } => {
            super::permissions::decline_window_over(runtime, tool, out)
        }
        Wake::DetachmentExpiry { turn_seq } => {
            if runtime
                .detached
                .as_ref()
                .is_some_and(|detachment| detachment.turn_seq == *turn_seq)
            {
                runtime.detached = None;
            }
        }
    }
}

/// How often a local request is checked for silence.
pub(in crate::backend) const SILENCE_TICK: Duration = Duration::from_secs(60);
/// Consecutive silent checks before the silence is published: five minutes.
const SILENCE_TICKS: i64 = 5;

/// Start watching a local request for silence.
pub(in crate::backend) fn watch_silence(turn_seq: i64, out: &mut ReduceOutput) {
    out.side_effects.push(SideEffect::Schedule {
        wake: Wake::SilenceTick { turn_seq },
        after: SILENCE_TICK,
    });
}

/// A prompt that is outstanding and an agent that does nothing look the same whether the agent
/// is mid-way through a long silent tool or its response was lost on the way. Neither is
/// failed on a guess. After five minutes of nothing the silence is published, and republished
/// every five minutes after, until the agent does something or the turn ends; the user decides,
/// and Esc cancels through the usual watchdog. A request waiting on the user is not silence.
fn silence_tick(runtime: &mut SessionRuntime, turn_seq: i64, out: &mut ReduceOutput) {
    let Some(turn) = runtime
        .turn
        .as_mut()
        .filter(|turn| turn.seq == turn_seq && matches!(turn.owner, TurnOwner::Local { .. }))
    else {
        return;
    };
    let work = &mut turn.work;
    if work.activity != work.silent_seen_activity || !work.pending_requests.is_empty() {
        work.silent_seen_activity = work.activity;
        work.silent_ticks = 0;
    } else {
        work.silent_ticks += 1;
        if work.silent_ticks >= SILENCE_TICKS && (work.silent_ticks - SILENCE_TICKS) % 5 == 0 {
            work.silent_published = true;
            out.side_effects.push(SideEffect::AnnounceSilent {
                silent_for_secs: work.silent_ticks * SILENCE_TICK.as_secs().cast_signed(),
            });
        }
    }
    watch_silence(turn_seq, out);
}
