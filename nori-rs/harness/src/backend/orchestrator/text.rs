//! What the agents are told: the delegation instructions, the hints that come back with task
//! results, and the text of a turn that delivers messages. Every string here states what is true
//! and what to do next.

use super::report::TaskSnapshot;

/// What the server tells agents about delegation, after its other instructions.
pub(crate) const TASK_INSTRUCTIONS: &str = nori_host_tools::DELEGATION_INSTRUCTIONS;

/// The hint returned for a task that started in the background.
pub(super) const DELIVERED_LATER: &str = "Started. Its result arrives as a new message when it finishes: keep working on something else \
     or end your turn. Do not poll.";

/// The hint returned for a task a wait gave up on, or one that is running.
pub(super) const STILL_RUNNING: &str = "Still running. Its result arrives as a new message when it finishes; do other work or end your \
     turn instead of waiting again.";

/// The hint for a wait that timed out.
pub(super) const WAIT_TIMED_OUT: &str = "The wait timed out; nothing was cancelled. Results arrive as new messages, so do other work or \
     end your turn rather than waiting again.";

/// Said after the same status was returned again and again.
pub(super) const STOP_POLLING: &str = "Stop polling: this has returned the same answer repeatedly and nothing changes while you wait. \
     End your turn now; the result or reply arrives as a new message.";

/// A message to a peer produces no result for its sender.
pub(super) const PEER_RESULT_NEVER_RETURNS: &str = "This was a message to a peer, not a delegated task: nothing returns to this task. If the \
     peer answers, the answer arrives as a new message.";

/// Waiting would hold the turn for something that only a new turn can deliver.
pub(super) const END_YOUR_TURN: &str = "Nothing will come back to this call. End your turn; a reply, if there is one, arrives as a new \
     message.";

/// Messages were taken from the caller's mailbox into the result of this call.
pub(super) const INBOX_ARRIVED: &str = "Messages for you arrived (see `inbox`) and are delivered with this result. Deal with them first; \
     answer with task_send.";

pub(super) const NO_TASK_ON_ACTOR: &str =
    "You have no task on this actor. Send it a message with task_send.";

pub(super) const ENDED_ACTOR: &str =
    "This actor's session ended. task_send to it revives it with its history.";

pub(super) const NOTHING_TO_CANCEL: &str = "You have nothing running on this actor.";

/// The child's agent is slow to open its session; the delegation is reported as it stands.
pub(super) const STILL_STARTING: &str = "The agent is still starting (agents that install themselves on first use are slow). The task \
     is queued on it and starts when it is ready; if it fails to start you are sent the failure as \
     a message. Do not poll.";

/// The error for a child whose session never opened.
pub(super) fn launch_failure(agent: &str, cause: &str) -> String {
    format!(
        "{agent} could not start, so nothing was delegated: {cause}. Check that the agent and the \
         helper programs it launches are installed and runnable, or choose another agent from \
         `agent_capabilities`."
    )
}

/// A revived actor had no journal to restore.
pub(super) const REVIVED_WITHOUT_HISTORY: &str = "The actor was restarted, but its earlier conversation was not kept, so it starts without \
     that history: restate anything it needs.";

/// A turn that ended with nothing to show for it.
pub(super) const NO_OUTPUT: &str = "the child ended its turn without writing anything or calling any tool. Its agent probably could \
     not start something it depends on (a helper program, a login, a model): check the agent's \
     installation and try again";

/// What a caller should do next about a task in the state `snapshot` reports.
pub(super) fn next_for(snapshot: &TaskSnapshot) -> Option<String> {
    match snapshot.status {
        "queued" => Some(format!(
            "Queued, not read yet: the actor is {} and takes it at its next turn boundary. Do not \
             wait for it; the result arrives as a new message.",
            snapshot.actor_state
        )),
        "starting" | "running" => Some(STILL_RUNNING.to_owned()),
        "delivered" => Some(
            "Handed to the recipient, which is not tracked as a task; it answers, if at all, with \
             its own message."
                .to_owned(),
        ),
        _ => None,
    }
}

/// How to answer a message from `sender`.
pub(super) fn reply_hint(sender: &str) -> String {
    format!("task_send(target=\"{sender}\", message=...)")
}

/// One message of a turn that delivers mail.
pub(super) struct Rendered {
    /// The sender's address; none for the user.
    pub(super) from: Option<String>,
    pub(super) id: String,
    pub(super) body: String,
}

/// The prompt of a turn that carries `messages`: the brief of a task that starts an actor, or the
/// messages other actors sent, one after the other.
pub(super) fn turn_prompt(brief: Option<&str>, messages: &[Rendered]) -> String {
    if let Some(brief) = brief {
        return brief.to_owned();
    }
    let mut text = String::new();
    let mut senders = Vec::new();
    for message in messages {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        match &message.from {
            Some(from) => {
                text.push_str(&format!(
                    "[Message from actor://{from}] ({})\n{}",
                    message.id, message.body
                ));
                if !senders.contains(from) {
                    senders.push(from.clone());
                }
            }
            None => text.push_str(&message.body),
        }
    }
    if !senders.is_empty() {
        text.push_str(&format!(
            "\n\nThese are messages from other actors, not from the user. To answer one, call \
             task_send with `target` set to its sender ({}); your final message in this turn is not \
             sent to them.",
            senders.join(", ")
        ));
    }
    text
}
