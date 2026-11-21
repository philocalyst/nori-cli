//! Which prompt starts when. Prompts wait while any turn is open, whoever owns it, and while a
//! session swap is in flight.

use tracing::debug;

use super::super::session_reducer::QueueOp;
use super::super::session_reducer::ReduceOutput;
use super::super::session_reducer::SideEffect;
use super::cancel;
use super::ledger;
use crate::normalized::ClientEvent;
use crate::normalized::QueueChanged;
use crate::normalized::WarningInfo;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;
use crate::normalized::session_runtime::SessionRuntime;
use crate::normalized::turn::TurnOwner;
use nori_protocol::acp::v1 as acp;

fn queue_changed(runtime: &SessionRuntime) -> ClientEvent {
    ClientEvent::QueueChanged(QueueChanged {
        prompts: runtime
            .queue
            .iter()
            .filter(|prompt| prompt.kind == QueuedPromptKind::User)
            .map(|prompt| {
                prompt
                    .display_text
                    .clone()
                    .unwrap_or_else(|| prompt.text.clone())
            })
            .collect(),
    })
}

pub(in crate::backend) fn submit(
    runtime: &mut SessionRuntime,
    prompt: QueuedPrompt,
    out: &mut ReduceOutput,
) {
    if runtime.turn.is_none() && !runtime.fenced {
        start_prompt(runtime, prompt, out);
        return;
    }
    out.queue_ops.push(QueueOp::Enqueued {
        prompt: prompt.clone(),
        front: false,
    });
    runtime.queue.push_back(prompt);
    debug!(
        target: "acp_event_flow",
        phase = runtime.phase_view().label(),
        queue_len = runtime.queue.len(),
        "Queued prompt while a turn is open"
    );
    out.events.push(queue_changed(runtime));
}

/// Put `prompt` first and interrupt the open turn; it starts as soon as that turn ends.
pub(in crate::backend) fn steer(
    runtime: &mut SessionRuntime,
    prompt: QueuedPrompt,
    out: &mut ReduceOutput,
) {
    let Some(turn) = runtime.turn.as_mut() else {
        if runtime.fenced {
            out.queue_ops.push(QueueOp::Enqueued {
                prompt: prompt.clone(),
                front: true,
            });
            runtime.queue.push_front(prompt);
            out.events.push(queue_changed(runtime));
        } else {
            start_prompt(runtime, prompt, out);
        }
        return;
    };
    turn.work.start_next_on_end = true;
    let keep_turn = turn.work.runs_subagent();
    out.queue_ops.push(QueueOp::Enqueued {
        prompt: prompt.clone(),
        front: true,
    });
    runtime.queue.push_front(prompt);
    out.events.push(queue_changed(runtime));
    interrupt_unless_subagents(runtime, keep_turn, out);
}

/// Interrupt the open turn so the prompt at the front starts now; but a turn with a subagent
/// running is let finish, since cancelling it would end the subagent's work too. The prompt
/// starts as the turn ends, and Esc still interrupts everything.
fn interrupt_unless_subagents(
    runtime: &mut SessionRuntime,
    keep_turn: bool,
    out: &mut ReduceOutput,
) {
    if keep_turn {
        out.events.push(ClientEvent::Warning(WarningInfo {
            message: "Subagents are running: your message goes in as soon as they finish. Esc interrupts them."
                .to_owned(),
        }));
    } else {
        cancel::cancel(runtime, None, out);
    }
}

/// Start the oldest queued prompt, interrupting the open turn if there is one.
pub(in crate::backend) fn promote(runtime: &mut SessionRuntime, out: &mut ReduceOutput) {
    // With nothing queued, the user sending "the next prompt" means the oldest one restored
    // from the previous session. It joins the queue it was always going to be in.
    if runtime.queue.is_empty()
        && let Some(restored) = runtime.held.pop_front()
    {
        runtime.queue.push_front(restored);
        out.events.push(queue_changed(runtime));
        out.side_effects.push(announce_restored(runtime));
    }
    if runtime.queue.is_empty() {
        return;
    }
    match runtime.turn.as_mut() {
        None => start_next_queued(runtime, out),
        Some(turn) => {
            turn.work.start_next_on_end = true;
            let keep_turn = turn.work.runs_subagent();
            interrupt_unless_subagents(runtime, keep_turn, out);
        }
    }
}

pub(in crate::backend) fn submit_if_idle(
    runtime: &mut SessionRuntime,
    prompt: QueuedPrompt,
    out: &mut ReduceOutput,
) {
    if runtime.turn.is_none() && runtime.queue.is_empty() && !runtime.fenced {
        start_prompt(runtime, prompt, out);
    } else {
        out.side_effects.push(SideEffect::RejectPromptBusy {
            event_id: prompt.event_id,
        });
    }
}

/// Start the front of the queue unless a turn is open.
pub(in crate::backend) fn start_next_queued(runtime: &mut SessionRuntime, out: &mut ReduceOutput) {
    if runtime.turn.is_some() || runtime.fenced {
        return;
    }
    if let Some(next) = runtime.queue.pop_front() {
        out.queue_ops.push(QueueOp::Started {
            event_id: next.event_id.clone(),
        });
        out.events.push(queue_changed(runtime));
        start_prompt(runtime, next, out);
    }
}

fn start_prompt(runtime: &mut SessionRuntime, prompt: QueuedPrompt, out: &mut ReduceOutput) {
    if super::pressure::compact_first(runtime, prompt.kind, out) {
        // The prompt waits first in line while the session compacts, and starts on the result.
        out.queue_ops.push(QueueOp::Enqueued {
            prompt: prompt.clone(),
            front: true,
        });
        runtime.queue.push_front(prompt);
        out.events.push(queue_changed(runtime));
        return;
    }
    let request_id = acp::RequestId::Str(uuid::Uuid::new_v4().to_string());
    if prompt.kind == QueuedPromptKind::User {
        // What the user asks starts a new task, in which the agent may be steered again.
        runtime.loops.new_task();
    }
    ledger::open_request_turn(
        runtime,
        TurnOwner::Local {
            request_id: request_id.clone(),
            prompt: Box::new(prompt.clone()),
            cancelling: false,
        },
    );
    if let Some(turn) = &runtime.turn {
        super::quiescence::watch_silence(turn.seq, out);
    }
    debug!(
        target: "acp_event_flow",
        %request_id,
        prompt_kind = ?prompt.kind,
        queue_len = runtime.queue.len(),
        "Reducer started prompt and emitted session/prompt side effect"
    );
    out.side_effects.push(SideEffect::SendPrompt {
        request_id,
        prompt: prompt.content,
    });
}

fn announce_restored(runtime: &SessionRuntime) -> SideEffect {
    SideEffect::AnnounceRestored {
        prompts: runtime
            .held
            .iter()
            .map(|prompt| {
                (
                    prompt.event_id.clone(),
                    prompt
                        .display_text
                        .clone()
                        .unwrap_or_else(|| prompt.text.clone()),
                )
            })
            .collect(),
    }
}

/// Hold the prompts the previous session had queued. They do not start by themselves.
pub(in crate::backend) fn restore(
    runtime: &mut SessionRuntime,
    prompts: Vec<QueuedPrompt>,
    out: &mut ReduceOutput,
) {
    runtime.held = prompts.into();
    out.side_effects.push(announce_restored(runtime));
}

/// Drop a queued or restored prompt by its id.
pub(in crate::backend) fn drop_queued(
    runtime: &mut SessionRuntime,
    event_id: &str,
    out: &mut ReduceOutput,
) {
    let queued = runtime.queue.len();
    runtime.queue.retain(|prompt| prompt.event_id != event_id);
    let held = runtime.held.len();
    runtime.held.retain(|prompt| prompt.event_id != event_id);
    if runtime.queue.len() != queued {
        out.events.push(queue_changed(runtime));
    }
    if runtime.held.len() != held {
        out.side_effects.push(announce_restored(runtime));
    }
    if runtime.queue.len() != queued || runtime.held.len() != held {
        out.queue_ops.push(QueueOp::Removed {
            event_id: event_id.to_owned(),
        });
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::backend::session_reducer::InboundEvent;
    use crate::backend::session_reducer::reduce;
    use crate::normalized::ClientEventNormalizer;

    fn prompt(id: &str) -> QueuedPrompt {
        QueuedPrompt {
            event_id: id.to_owned(),
            kind: QueuedPromptKind::User,
            text: id.to_owned(),
            prompt_meta: None,
            original_content: Vec::new(),
            content: Vec::new(),
            context: Default::default(),
            display_text: None,
        }
    }

    fn cancels(out: &ReduceOutput) -> bool {
        out.side_effects
            .iter()
            .any(|effect| matches!(effect, SideEffect::SendCancel { .. }))
    }

    /// Sending the next message now interrupts a turn, unless a subagent runs in it: then the
    /// message waits for the turn, and the subagent's work is not thrown away.
    #[test]
    fn sending_now_waits_for_a_running_subagent_instead_of_cancelling_it() {
        let mut runtime = SessionRuntime::new();
        let mut normalizer = ClientEventNormalizer::default();
        reduce(
            &mut runtime,
            InboundEvent::PromptSubmit(prompt("first")),
            &mut normalizer,
        );
        let subagent = acp::ToolCall::new(acp::ToolCallId::new("agent-1"), "Explore the repo")
            .status(acp::ToolCallStatus::InProgress)
            .meta(nori_host_tools::subagent_meta());
        reduce(
            &mut runtime,
            InboundEvent::Notification(Box::new(acp::SessionUpdate::ToolCall(subagent))),
            &mut normalizer,
        );
        let steered = reduce(
            &mut runtime,
            InboundEvent::PromptSteer(prompt("second")),
            &mut normalizer,
        );
        assert_eq!(
            (
                cancels(&steered),
                runtime.queue.front().map(|queued| queued.event_id.clone()),
                runtime
                    .turn
                    .as_ref()
                    .is_some_and(|turn| turn.work.start_next_on_end),
            ),
            (false, Some("second".to_owned()), true)
        );

        // Once the subagent is done, sending now interrupts as always.
        reduce(
            &mut runtime,
            InboundEvent::Notification(Box::new(acp::SessionUpdate::ToolCallUpdate(
                acp::ToolCallUpdate::new(
                    acp::ToolCallId::new("agent-1"),
                    acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::Completed),
                ),
            ))),
            &mut normalizer,
        );
        let promoted = reduce(&mut runtime, InboundEvent::PromoteQueued, &mut normalizer);
        assert!(cancels(&promoted));
    }
}
