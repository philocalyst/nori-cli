//! Properties that must hold in every world, whatever its script.
//!
//! They are checked over the client's event stream and the agents' flight recorder together, so
//! a violation is a fact about how the harness treated what the agents did.

use std::collections::HashMap;

use nori_fake_agent::Entry;
use nori_fake_agent::Record;
use nori_fake_agent::world::Answer;
use nori_protocol::acp::v1 as acp;

use crate::events::Events;

/// One property that did not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub invariant: &'static str,
    pub detail: String,
}

impl std::fmt::Display for Violation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.invariant, self.detail)
    }
}

/// What to check: some properties only hold once the session has settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// The world has no work in flight, so unfinished things are failures and not progress.
    pub settled: bool,
}

/// Every violation of every invariant.
pub fn check(events: &Events, recorder: &[Entry], check: Check) -> Vec<Violation> {
    let mut violations = Vec::new();
    no_permission_cancelled_while_its_turn_is_open(recorder, &mut violations);
    no_message_delivered_twice(recorder, &mut violations);
    each_response_arrives_once(events, &mut violations);
    if check.settled {
        no_ghost_running_tools(events, &mut violations);
        every_message_delivered(recorder, &mut violations);
        every_turn_ended(recorder, &mut violations);
    }
    violations
}

/// A permission request is answered `Cancelled` only because the user cancelled the turn that
/// owns it (or shut the session down, or the connection died): the last thing the user did before
/// the answer, since their last prompt, is a cancel. Anything else is the harness giving up on a
/// question it had no right to withdraw (the root cause of an agent retrying a refused edit for
/// hours). A cancel opens a window that lasts until the user's next prompt, so an agent that keeps
/// asking after being cancelled is declined, not silently obeyed and not left hanging.
fn no_permission_cancelled_while_its_turn_is_open(recorder: &[Entry], out: &mut Vec<Violation>) {
    let mut asked: HashMap<(String, String), i64> = HashMap::new();
    for entry in recorder {
        match &entry.record {
            Record::PermissionAsked { session, tool, .. } => {
                asked.insert((session.clone(), tool.clone()), entry.seq);
            }
            Record::PermissionAnswered {
                agent,
                session,
                tool,
                answer: Answer::Cancelled,
            } => {
                let Some(asked_at) = asked.remove(&(session.clone(), tool.clone())) else {
                    continue;
                };
                let last_user_action = recorder
                    .iter()
                    .filter(|earlier| earlier.seq < entry.seq)
                    .filter_map(|earlier| match &earlier.record {
                        Record::User { action, .. } => Some(action.as_str()),
                        _ => None,
                    })
                    .rfind(|action| {
                        matches!(
                            *action,
                            "prompt"
                                | "steer"
                                | "cancel"
                                | "shutdown"
                                | "stop_agent"
                                | "resume"
                                | "rewind"
                        )
                    });
                let connection_died = recorder.iter().any(|between| {
                    between.seq > asked_at
                        && between.seq < entry.seq
                        && matches!(between.record, Record::Crashed { .. })
                });
                let justified = connection_died
                    || matches!(
                        last_user_action,
                        // A rewind cancels the running turn before it swaps the session.
                        Some("cancel" | "shutdown" | "stop_agent" | "resume" | "rewind")
                    );
                if !justified {
                    out.push(Violation {
                        invariant: "no_permission_cancelled_while_its_turn_is_open",
                        detail: format!(
                            "{agent}/{session}: the permission request for {tool:?} was answered \
                             as cancelled, but the user had not cancelled, shut down or lost the connection"
                        ),
                    });
                }
            }
            Record::PermissionAnswered { session, tool, .. } => {
                asked.remove(&(session.clone(), tool.clone()));
            }
            _ => {}
        }
    }
}

/// The messages agents sent each other with `task_send`, as `(recipient, text)`.
fn sent_messages(recorder: &[Entry]) -> Vec<(String, String)> {
    recorder
        .iter()
        .filter_map(|entry| match &entry.record {
            Record::HostTool {
                name,
                arguments,
                is_error: false,
                ..
            } if name == "task_send" => Some((
                arguments["to"].as_str().unwrap_or_default().to_owned(),
                arguments["message"].as_str().unwrap_or_default().to_owned(),
            )),
            _ => None,
        })
        .collect()
}

fn deliveries_of(recorder: &[Entry], message: &str) -> usize {
    recorder
        .iter()
        .filter(|entry| match &entry.record {
            Record::PromptReceived { text, .. } => text.contains(message),
            _ => false,
        })
        .count()
}

fn no_message_delivered_twice(recorder: &[Entry], out: &mut Vec<Violation>) {
    let sent = sent_messages(recorder);
    for (to, message) in &sent {
        let sent_times = sent.iter().filter(|(_, other)| other == message).count();
        let delivered = deliveries_of(recorder, message);
        if delivered > sent_times {
            out.push(Violation {
                invariant: "no_message_delivered_twice",
                detail: format!(
                    "the message to {to} ({message:?}) was sent {sent_times} time(s) and reached an agent {delivered} times"
                ),
            });
        }
    }
}

fn every_message_delivered(recorder: &[Entry], out: &mut Vec<Violation>) {
    for (to, message) in sent_messages(recorder) {
        if deliveries_of(recorder, &message) == 0 {
            out.push(Violation {
                invariant: "every_message_delivered",
                detail: format!(
                    "the message to {to} ({message:?}) was accepted by task_send and never reached an agent"
                ),
            });
        }
    }
}

/// The client sees each prompt request answered at most once, whatever the agent put on the wire.
fn each_response_arrives_once(events: &Events, out: &mut Vec<Violation>) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (id, _) in events.responses() {
        *seen.entry(id.to_string()).or_default() += 1;
    }
    for (id, count) in seen {
        if count > 1 {
            out.push(Violation {
                invariant: "each_response_arrives_once",
                detail: format!("request {id} was answered {count} times in the event stream"),
            });
        }
    }
}

/// When nothing is running, no tool call is still shown as running.
fn no_ghost_running_tools(events: &Events, out: &mut Vec<Violation>) {
    let mut lanes = vec![("root".to_owned(), events.root())];
    for info in events.subagents() {
        lanes.push((
            info.title
                .clone()
                .unwrap_or_else(|| info.session_id.to_string()),
            events.child(&info.session_id),
        ));
    }
    for (name, lane) in lanes {
        for call in lane.tool_calls() {
            if matches!(
                call.status,
                acp::ToolCallStatus::Pending | acp::ToolCallStatus::InProgress
            ) {
                out.push(Violation {
                    invariant: "no_ghost_running_tools",
                    detail: format!(
                        "{name}: tool call {} ({:?}) is still {:?} though the session settled",
                        call.id, call.title, call.status
                    ),
                });
            }
        }
    }
}

/// Every prompt an agent received had its turn end (an agent that hangs on purpose has a scene
/// that says so; this reports the ones that did not).
fn every_turn_ended(recorder: &[Entry], out: &mut Vec<Violation>) {
    let mut open: HashMap<String, i64> = HashMap::new();
    for entry in recorder {
        match &entry.record {
            Record::PromptReceived { session, .. } => {
                *open.entry(session.clone()).or_default() += 1
            }
            Record::TurnEnded { session, .. } => {
                if let Some(count) = open.get_mut(session) {
                    *count -= 1;
                }
            }
            _ => {}
        }
    }
    for (session, count) in open {
        if count > 0 {
            out.push(Violation {
                invariant: "every_turn_ended",
                detail: format!("{session}: {count} prompt(s) never ended their turn"),
            });
        }
    }
}
