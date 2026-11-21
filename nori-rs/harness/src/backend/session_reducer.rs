//! Serialized reducer for the session's turn ledger.
//!
//! All inbound ACP traffic for a session flows through [`reduce()`], which mutates the
//! [`SessionRuntime`] and produces [`ClientEvent`]s. The caller executes any [`SideEffect`]s
//! after reduction. The ledger rules live in [`super::turn`]; this module is the event
//! vocabulary and the dispatcher.

use std::collections::BTreeSet;
use std::time::Duration;

use super::prompt_lifecycle::PromptResult;
use super::turn::cancel;
use super::turn::lifecycle;
use super::turn::metadata;
use super::turn::permissions;
use super::turn::queue;
use super::turn::quiescence;
use super::turn::updates;
use crate::normalized::ClientEvent;
use crate::normalized::ClientEventNormalizer;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::SessionRuntime;
use nori_protocol::acp::v1 as acp;

/// A deferred wake-up the reducer asked the driver to deliver. Every wake names the turn it was
/// scheduled for, so one that arrives after that turn ended or changed is inert.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wake {
    /// The agent turn may have gone quiet. `activity` is the turn's activity count when armed.
    QuietWindow { turn_seq: i64, activity: i64 },
    /// An agent turn the user cancelled did not stop in time.
    CancelDeadline { turn_seq: i64 },
    /// The window in which activity still counts as continuing an ended turn is over.
    DetachmentExpiry { turn_seq: i64 },
    /// Publish how many requests for `tool` were declined while the first notice stood.
    DeclineSummary { tool: String },
    /// Check whether a local request's agent has gone silent.
    SilenceTick { turn_seq: i64 },
}

/// Which turn a `session/cancel` is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelTarget {
    /// Our `session/prompt`.
    Request(acp::RequestId),
    /// An agent turn, by ledger sequence number.
    AgentTurn(i64),
}

/// Everything that can affect [`SessionRuntime`] state.
#[derive(Debug)]
pub enum InboundEvent {
    /// A terminal outcome owned by one ACP wire request. A result whose request does not own
    /// the current turn (late, duplicated, superseded) is inert.
    PromptResultFor {
        request_id: acp::RequestId,
        result: PromptResult,
    },
    /// A `session/update` notification from the agent.
    Notification(Box<acp::SessionUpdate>),
    /// The response to the `session/load` that owns the current replay turn.
    LoadResponse { request_id: acp::RequestId },
    /// The transport has assigned the active prompt its ACP wire request ID.
    PromptStarted { request_id: acp::RequestId },
    /// A permission or question request from the agent. Valid in every turn.
    PermissionRequest {
        request_id: String,
        /// The tool call the request is about; `None` for a question.
        call_id: Option<String>,
        title: String,
    },
    /// The user (or approval policy) answered a request, or it was withdrawn.
    RequestAnswered { request_id: String },
    /// The user submitted a prompt (may be queued if a turn is open).
    PromptSubmit(QueuedPrompt),
    /// Prioritize this prompt and interrupt the current turn.
    PromptSteer(QueuedPrompt),
    /// Interrupt the current turn and start the oldest queued prompt.
    PromoteQueued,
    /// A remote controller submitted a prompt that must not queue.
    PromptSubmitIfIdle(QueuedPrompt),
    /// The user requested cancellation of the current turn.
    CancelSubmit,
    /// A controller requested cancellation only if it owns this prompt.
    CancelSubmitFor { request_id: acp::RequestId },
    /// A `session/load` was initiated.
    LoadSubmit { request_id: acp::RequestId },
    /// A scheduled wake-up fired. `live_requests` is every request the session still has
    /// outstanding with the agent, so requests withdrawn without an answer stop blocking.
    Woke {
        wake: Wake,
        live_requests: BTreeSet<String>,
    },
    /// The connection to the agent is gone: nothing can finish.
    ConnectionLost,
    /// The failed attempt of the local turn with this ledger sequence number will be retried or
    /// moved to another route: what it left running is over.
    AttemptEnded { turn_seq: i64 },
    /// A session swap finished: prompts may start again.
    ReleaseFence,
    /// The session may have come to rest after a turn: compact now if its context is past the
    /// trigger, or prepare the summary if it is close (see [`super::turn::pressure`]).
    CompactIfDue,
    /// Prompts typed ahead before the previous session ended: held for the user, not queued.
    RestoreQueued(Vec<QueuedPrompt>),
    /// The user dropped a queued or restored prompt.
    DropQueued { event_id: String },
}

pub(super) fn inbound_event_kind(event: &InboundEvent) -> &'static str {
    match event {
        InboundEvent::Notification(update) => crate::connection::session_update_kind(update),
        InboundEvent::PromptResultFor { result, .. } => match result {
            PromptResult::Response(_) => "prompt_response",
            PromptResult::Failed(_) => "prompt_failed",
        },
        InboundEvent::LoadResponse { .. } => "load_response",
        InboundEvent::PromptStarted { .. } => "prompt_started",
        InboundEvent::PermissionRequest { .. } => "permission_request",
        InboundEvent::RequestAnswered { .. } => "request_answered",
        InboundEvent::PromptSubmit(_)
        | InboundEvent::PromptSubmitIfIdle(_)
        | InboundEvent::PromptSteer(_) => "prompt_submit",
        InboundEvent::PromoteQueued => "promote_queued",
        InboundEvent::CancelSubmit | InboundEvent::CancelSubmitFor { .. } => "cancel_submit",
        InboundEvent::LoadSubmit { .. } => "load_submit",
        InboundEvent::Woke { .. } => "woke",
        InboundEvent::ConnectionLost => "connection_lost",
        InboundEvent::AttemptEnded { .. } => "attempt_ended",
        InboundEvent::ReleaseFence => "release_fence",
        InboundEvent::CompactIfDue => "compact_if_due",
        InboundEvent::RestoreQueued(_) => "restore_queued",
        InboundEvent::DropQueued { .. } => "drop_queued",
    }
}

/// Side effects the caller must execute after reduction.
#[derive(Debug, PartialEq)]
pub enum SideEffect {
    /// Send a `session/prompt` to the agent.
    SendPrompt {
        request_id: acp::RequestId,
        prompt: Vec<acp::ContentBlock>,
    },
    /// Send a `session/cancel` notification to the agent. `arm_watchdog` is set the first time a
    /// local request is cancelled: if the agent ignores it Nori ends the request itself.
    SendCancel {
        target: CancelTarget,
        arm_watchdog: bool,
    },
    /// Answer a pending permission request as cancelled.
    ResolvePermissionCancelled { request_id: String },
    /// Reject a prompt instead of adding it to the queue.
    RejectPromptBusy { event_id: String },
    /// Deliver `wake` back to the reducer after `after`.
    Schedule { wake: Wake, after: Duration },
    /// Tell the user what the permission request `request_id` is: the agent continued after its
    /// turn ended.
    AnnounceContinued {
        request_id: String,
        tool: String,
        cause: nori_protocol::AgentTurnCause,
        after_cancel: bool,
    },
    /// Tell the user that `count` requests for `tool` were declined because their turn was
    /// cancelled.
    AnnounceDeclined { tool: String, count: i64 },
    /// The prompts restored from the previous session that still wait for the user, as
    /// `(id, text)`.
    AnnounceRestored { prompts: Vec<(String, String)> },
    /// The agent has done nothing for this long while a prompt is outstanding.
    AnnounceSilent { silent_for_secs: i64 },
    /// The agent that was silent did something.
    AnnounceHeard,
    /// Compact the session before anything starts; the ledger holds the fence until whoever
    /// performs it releases it.
    CompactFirst {
        usage: crate::normalized::session_runtime::ContextUsage,
        how: super::turn::pressure::CompactHow,
    },
    /// Write the summary now and keep it for when the context reaches the trigger.
    PrepareSummary {
        usage: crate::normalized::session_runtime::ContextUsage,
    },
}

/// A change to the queue of typed-ahead prompts, for the journal: replaying them rebuilds what
/// was queued if the session ends first.
#[derive(Debug, Clone)]
pub enum QueueOp {
    Enqueued { prompt: QueuedPrompt, front: bool },
    Started { event_id: String },
    Removed { event_id: String },
}

/// The local request a turn ended, with the prompt that started it.
#[derive(Debug, Clone)]
pub struct CompletedTurn {
    pub request_id: acp::RequestId,
    pub prompt: QueuedPrompt,
    pub stop_reason: acp::StopReason,
    pub last_agent_message: Option<String>,
}

/// An agent turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentTurnEnded {
    /// The agent did not say so itself, so observers are told the session went idle.
    pub announce_idle: bool,
    /// The user cancelled it or the connection was lost: nothing should be continued from it.
    pub interrupted: bool,
}

/// The output of a single reduction step.
#[derive(Default)]
pub struct ReduceOutput {
    pub events: Vec<ClientEvent>,
    pub side_effects: Vec<SideEffect>,
    pub completed: Option<CompletedTurn>,
    pub agent_ended: Option<AgentTurnEnded>,
    pub queue_ops: Vec<QueueOp>,
}

/// Process one inbound event, mutating the session runtime and producing client events and side
/// effects.
pub fn reduce(
    runtime: &mut SessionRuntime,
    event: InboundEvent,
    normalizer: &mut ClientEventNormalizer,
) -> ReduceOutput {
    let mut out = ReduceOutput::default();
    match event {
        InboundEvent::PromptResultFor { request_id, result } => {
            lifecycle::prompt_result(runtime, &request_id, result, &mut out);
        }
        InboundEvent::PromptSubmit(prompt) => queue::submit(runtime, prompt, &mut out),
        InboundEvent::PromptSteer(prompt) => queue::steer(runtime, prompt, &mut out),
        InboundEvent::PromoteQueued => queue::promote(runtime, &mut out),
        InboundEvent::PromptSubmitIfIdle(prompt) => {
            queue::submit_if_idle(runtime, prompt, &mut out);
        }
        InboundEvent::PromptStarted { request_id } => {
            lifecycle::prompt_started(runtime, request_id, &mut out);
        }
        InboundEvent::CancelSubmit => cancel::cancel(runtime, None, &mut out),
        InboundEvent::CancelSubmitFor { request_id } => {
            cancel::cancel(runtime, Some(&request_id), &mut out);
        }
        InboundEvent::LoadSubmit { request_id } => {
            lifecycle::load_submit(runtime, request_id, &mut out);
        }
        InboundEvent::LoadResponse { request_id } => {
            lifecycle::load_response(runtime, &request_id, &mut out);
        }
        InboundEvent::Notification(update) => {
            if nori_protocol::is_interruption_close(&update) {
                // The session's own bookkeeping for a tool call it closed. Never activity.
            } else if metadata::is_session_metadata(&update) {
                metadata::reduce(runtime, &update, normalizer, &mut out);
            } else if updates::is_turn_activity(&update) {
                updates::reduce(runtime, &update, normalizer, &mut out);
            }
        }
        InboundEvent::PermissionRequest {
            request_id,
            call_id,
            title,
        } => permissions::opened(runtime, request_id, call_id, &title, &mut out),
        InboundEvent::RequestAnswered { request_id } => {
            permissions::answered(runtime, &request_id, &mut out);
        }
        InboundEvent::Woke {
            wake,
            live_requests,
        } => quiescence::woke(runtime, &wake, &live_requests, &mut out),
        InboundEvent::ConnectionLost => lifecycle::connection_lost(runtime, &mut out),
        InboundEvent::AttemptEnded { turn_seq } => {
            lifecycle::attempt_ended(runtime, turn_seq, &mut out);
        }
        InboundEvent::ReleaseFence => {
            runtime.fenced = false;
            queue::start_next_queued(runtime, &mut out);
        }
        InboundEvent::CompactIfDue => super::turn::pressure::at_rest(runtime, &mut out),
        InboundEvent::RestoreQueued(prompts) => queue::restore(runtime, prompts, &mut out),
        InboundEvent::DropQueued { event_id } => queue::drop_queued(runtime, &event_id, &mut out),
    }
    out
}
