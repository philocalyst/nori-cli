//! Context pressure: the ledger compacts the session before its context runs out.
//!
//! The context the agent last reported (`SessionRuntime::context_usage`) is measured against the
//! session's `compaction` policy (`docs/workflows.md`) at two moments, never mid-turn:
//!
//! - **A prompt is about to start** (any prompt but a compaction's own). When the context is past
//!   the trigger, the prompt goes back to the front of the queue, the ledger holds the fence, and
//!   [`SideEffect::CompactFirst`] asks the backend to compact. The prompt starts on the compacted
//!   session.
//! - **The session comes to rest** after a turn ([`InboundEvent::CompactIfDue`], sent once the
//!   turn's follow-ups had their chance). Past the trigger, the compaction starts now, while the
//!   provider's prompt cache is warm and the person is reading. Within `background_margin_tokens`
//!   of it, an agent that compacts by summary writes the summary now and keeps it
//!   ([`SideEffect::PrepareSummary`]); when the trigger is reached, the switch is instant.
//!
//! A failed automatic compaction is said once and the session stops compacting itself until the
//! person compacts by hand. The thresholds and the precompute lead follow OpenCode's
//! `session/compaction.ts` (MIT, `sst/opencode`); the ledger placement is Nori's own.
//!
//! [`InboundEvent::CompactIfDue`]: super::super::session_reducer::InboundEvent::CompactIfDue

use nori_workflow::CompactionPolicy;
use nori_workflow::ContextPressure;

use super::super::session_reducer::ReduceOutput;
use super::super::session_reducer::SideEffect;
use crate::normalized::session_runtime::ContextUsage;
use crate::normalized::session_runtime::QueuedPromptKind;
use crate::normalized::session_runtime::SessionRuntime;

/// How the backend compacts when the ledger asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactHow {
    /// The agent's own `/compact`, as an ordinary turn in the same session.
    Native,
    /// Swap to a new session that opens with the summary prepared earlier, when the context was
    /// `used_tokens` full, if it is still the session's.
    Prepared { used_tokens: i64 },
    /// Ask for a summary now, then swap.
    Summary,
}

/// Where automatic compaction stands in this session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Watching,
    /// A summary is being written ahead of need.
    Preparing,
    /// A summary is kept, written when the context was `used_tokens` full.
    Prepared { used_tokens: i64 },
    /// The queue waits behind a compaction the ledger asked for.
    Compacting,
    /// An automatic compaction failed: none starts again until the person compacts.
    BackedOff,
}

/// How a compaction ended, as the ledger needs to know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionEnd {
    /// The context is smaller now; what the agent last reported no longer describes it.
    Compacted,
    /// A summary was written ahead of need and kept.
    Prepared,
    /// Nothing changed.
    Failed,
}

/// The session's compaction policy and what automatic compaction has done in it.
#[derive(Debug, Clone, Default)]
pub struct PressureGuard {
    pub policy: CompactionPolicy,
    phase: Phase,
}

impl PressureGuard {
    /// A compaction of any origin ended. Returns whether it was one the ledger started and it
    /// failed, so the person is told automatic compaction stopped.
    pub(in crate::backend) fn ended(&mut self, end: CompactionEnd, used_tokens: i64) -> bool {
        let automatic = matches!(self.phase, Phase::Compacting | Phase::Preparing);
        self.phase = match end {
            CompactionEnd::Compacted => Phase::Watching,
            CompactionEnd::Prepared => Phase::Prepared { used_tokens },
            CompactionEnd::Failed if automatic => Phase::BackedOff,
            CompactionEnd::Failed => self.phase,
        };
        automatic && end == CompactionEnd::Failed
    }

    /// A summary turn the ledger did not ask for is starting (the person's `/compact`): it is a
    /// compaction like any other, and a summary prepared earlier is out of date once it swaps.
    fn manual(&mut self) {
        if matches!(self.phase, Phase::BackedOff) {
            self.phase = Phase::Watching;
        }
    }
}

/// Before `kind` starts: when the context is past the trigger, hold the queue and ask for a
/// compaction first. Returns whether the prompt must wait.
pub(in crate::backend) fn compact_first(
    runtime: &mut SessionRuntime,
    kind: QueuedPromptKind,
    out: &mut ReduceOutput,
) -> bool {
    match kind {
        QueuedPromptKind::Compact | QueuedPromptKind::NativeCompact => {
            if matches!(runtime.compaction.phase, Phase::Watching | Phase::BackedOff) {
                runtime.compaction.manual();
            }
            return false;
        }
        QueuedPromptKind::User
        | QueuedPromptKind::GoalContinuation
        | QueuedPromptKind::TaskResults
        | QueuedPromptKind::MonitorEvents
        | QueuedPromptKind::WorkflowHook
        | QueuedPromptKind::LoopNudge
        | QueuedPromptKind::GateContinue => {}
    }
    let Some(usage) = runtime.context_usage else {
        return false;
    };
    if pressure(runtime, usage) != Some(ContextPressure::Due) {
        return false;
    }
    request(runtime, usage, out);
    true
}

/// The session came to rest after a turn: compact now if the context is past the trigger, or
/// prepare the summary if it is close.
pub(in crate::backend) fn at_rest(runtime: &mut SessionRuntime, out: &mut ReduceOutput) {
    if runtime.turn.is_some() || !runtime.queue.is_empty() || runtime.fenced {
        return;
    }
    let Some(usage) = runtime.context_usage else {
        return;
    };
    match pressure(runtime, usage) {
        Some(ContextPressure::Due) => request(runtime, usage, out),
        Some(ContextPressure::Approaching)
            if runtime.compaction.phase == Phase::Watching && !native(runtime) =>
        {
            runtime.compaction.phase = Phase::Preparing;
            out.side_effects.push(SideEffect::PrepareSummary { usage });
        }
        Some(ContextPressure::Approaching | ContextPressure::Comfortable) | None => {}
    }
}

/// Where the context stands, when automatic compaction may act on it at all.
fn pressure(runtime: &SessionRuntime, usage: ContextUsage) -> Option<ContextPressure> {
    let guard = &runtime.compaction;
    let free = match guard.phase {
        Phase::Watching | Phase::Prepared { .. } => true,
        Phase::Preparing | Phase::Compacting | Phase::BackedOff => false,
    };
    (guard.policy.auto && free).then(|| {
        guard
            .policy
            .pressure(usage.used_tokens, usage.window_tokens)
    })
}

fn native(runtime: &SessionRuntime) -> bool {
    runtime.compaction.policy.prefer_native
        && runtime
            .available_commands
            .iter()
            .any(|command| command.name == "compact")
}

fn request(runtime: &mut SessionRuntime, usage: ContextUsage, out: &mut ReduceOutput) {
    let how = if native(runtime) {
        CompactHow::Native
    } else if let Phase::Prepared { used_tokens } = runtime.compaction.phase {
        CompactHow::Prepared { used_tokens }
    } else {
        CompactHow::Summary
    };
    runtime.compaction.phase = Phase::Compacting;
    runtime.fenced = true;
    out.side_effects
        .push(SideEffect::CompactFirst { usage, how });
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::normalized::AgentCommandInfo;

    fn runtime(used_tokens: i64) -> SessionRuntime {
        let mut runtime = SessionRuntime::new();
        runtime.context_usage = Some(ContextUsage {
            used_tokens,
            window_tokens: 200_000,
        });
        runtime
    }

    fn effects(out: ReduceOutput) -> Vec<SideEffect> {
        out.side_effects
    }

    #[test]
    fn a_prompt_past_the_trigger_waits_for_a_compaction() {
        // A 200k window compacts below 30k free (15%), and prepares below 42k free (the 12k margin).
        let mut runtime = runtime(175_000);
        let mut out = ReduceOutput::default();
        assert!(compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
        assert!(runtime.fenced);
        assert_eq!(
            effects(out),
            vec![SideEffect::CompactFirst {
                usage: runtime.context_usage.unwrap_or(ContextUsage {
                    used_tokens: 0,
                    window_tokens: 0
                }),
                how: CompactHow::Summary,
            }]
        );
        // While it runs, nothing asks again; the compaction's own prompt starts.
        let mut out = ReduceOutput::default();
        assert!(!compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
        assert!(!compact_first(
            &mut runtime,
            QueuedPromptKind::Compact,
            &mut out
        ));
        assert_eq!(effects(out), Vec::new());
    }

    #[test]
    fn a_prompt_with_room_starts_at_once() {
        let mut runtime = runtime(100_000);
        let mut out = ReduceOutput::default();
        assert!(!compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
        assert_eq!(effects(out), Vec::new());
    }

    #[test]
    fn an_agent_with_its_own_compact_is_asked_to_use_it() {
        let mut runtime = runtime(190_000);
        runtime.available_commands.push(AgentCommandInfo {
            name: "compact".to_owned(),
            description: String::new(),
            input_hint: None,
        });
        let mut out = ReduceOutput::default();
        assert!(compact_first(
            &mut runtime,
            QueuedPromptKind::GoalContinuation,
            &mut out
        ));
        assert!(matches!(
            effects(out).as_slice(),
            [SideEffect::CompactFirst {
                how: CompactHow::Native,
                ..
            }]
        ));
    }

    #[test]
    fn a_session_close_to_the_trigger_prepares_once_and_then_uses_it() {
        let mut runtime = runtime(160_000);
        let mut out = ReduceOutput::default();
        at_rest(&mut runtime, &mut out);
        assert_eq!(
            effects(out),
            vec![SideEffect::PrepareSummary {
                usage: ContextUsage {
                    used_tokens: 160_000,
                    window_tokens: 200_000
                }
            }]
        );
        assert!(!runtime.compaction.ended(CompactionEnd::Prepared, 162_000));
        let mut out = ReduceOutput::default();
        at_rest(&mut runtime, &mut out);
        assert_eq!(
            effects(out),
            Vec::new(),
            "one summary is prepared, not one a turn"
        );

        runtime.context_usage = Some(ContextUsage {
            used_tokens: 175_000,
            window_tokens: 200_000,
        });
        let mut out = ReduceOutput::default();
        at_rest(&mut runtime, &mut out);
        assert!(matches!(
            effects(out).as_slice(),
            [SideEffect::CompactFirst {
                how: CompactHow::Prepared {
                    used_tokens: 162_000
                },
                ..
            }]
        ));
    }

    #[test]
    fn a_failed_automatic_compaction_stops_until_the_person_compacts() {
        let mut runtime = runtime(190_000);
        let mut out = ReduceOutput::default();
        assert!(compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
        assert!(runtime.compaction.ended(CompactionEnd::Failed, 190_000));
        runtime.fenced = false;
        let mut out = ReduceOutput::default();
        assert!(!compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
        at_rest(&mut runtime, &mut out);
        assert_eq!(effects(out), Vec::new());

        // The person's own `/compact` starts automatic compaction again.
        let mut out = ReduceOutput::default();
        assert!(!compact_first(
            &mut runtime,
            QueuedPromptKind::Compact,
            &mut out
        ));
        assert!(!runtime.compaction.ended(CompactionEnd::Compacted, 0));
        assert!(compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
    }

    #[test]
    fn policy_off_never_compacts() {
        let mut runtime = runtime(199_000);
        runtime.compaction.policy.auto = false;
        let mut out = ReduceOutput::default();
        assert!(!compact_first(
            &mut runtime,
            QueuedPromptKind::User,
            &mut out
        ));
        at_rest(&mut runtime, &mut out);
        assert_eq!(effects(out), Vec::new());
    }
}
