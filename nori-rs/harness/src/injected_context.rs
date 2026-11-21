//! Telling what a person asked from what Nori (or an agent on its behalf) wrapped around it.
//!
//! Nori puts blocks of its own in front of a prompt: the session preamble, the cached agent
//! catalog, goal context, workflow rules, a compaction checkpoint, delegated results. They reach
//! the agent as assistant-only blocks (see `backend::prompt_context`), but an agent that
//! titles its sessions from the message it received, or echoes it back, repeats them. Anything
//! that names a session by what was asked goes through [`user_words`].
//!
//! [`INJECTED_TAGS`] is the one list of the wrapper tags. Add a tag here when something that
//! injects context starts producing one. (What a hook prints as `::context::` is whatever the
//! hook wrote and carries no tag; it is an assistant-only block of its own on the wire.)

/// The wrapper tags of every block injected around a user's words, and where each is produced.
pub const INJECTED_TAGS: &[&str] = &[
    // The session preamble (`tui/session_context*.md`) and attached resources (`claude-acp`).
    "context",
    // `backend::orchestrator::capabilities`.
    "nori_agent_catalog",
    // `backend::thread_goal`.
    "goal_context",
    "goal_control",
    // `backend::workflow`.
    "workflow",
    // `backend::orchestrator::wake`.
    "delegated_task_results",
    // `compact`.
    "conversation-checkpoint",
    "recent-context",
    // Reminders agents and their adapters add to a user message.
    "system-reminder",
    "environment_context",
    "user_instructions",
];

/// The words in `text` once every injected block is removed and whitespace is collapsed to
/// single spaces. Empty when nothing was asked.
pub fn user_words(text: &str) -> String {
    without_injected(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// `text` with every injected block removed and the rest left as written. A block that was cut
/// off before its closing tag, as an agent's truncated title leaves it, is removed to the end.
pub fn without_injected(text: &str) -> String {
    let mut kept = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        kept.push_str(&rest[..open]);
        let from_open = &rest[open..];
        let Some(tag) = injected_tag_at(from_open) else {
            kept.push('<');
            rest = &rest[open + 1..];
            continue;
        };
        let Some(end_of_open) = from_open.find('>') else {
            // The opening tag itself was cut off.
            rest = "";
            break;
        };
        if from_open[..end_of_open].ends_with('/') {
            rest = &from_open[end_of_open + 1..];
            continue;
        }
        let close = format!("</{tag}>");
        rest = match from_open.find(&close) {
            Some(close_at) => &from_open[close_at + close.len()..],
            None => "",
        };
        // Words on either side of a removed block stay apart.
        kept.push(' ');
    }
    kept.push_str(rest);
    kept.trim().to_owned()
}

/// The injected tag `text` opens with, if it opens with one: `<tag>`, `<tag attr="…">` or
/// `<tag/>`, and not a longer name that merely starts the same.
fn injected_tag_at(text: &str) -> Option<&'static str> {
    let after = text.strip_prefix('<')?;
    INJECTED_TAGS.iter().copied().find(|tag| {
        after.strip_prefix(tag).is_some_and(|tail| {
            tail.chars()
                .next()
                .is_none_or(|next| next == '>' || next == '/' || next.is_whitespace())
        })
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn only_the_listed_tags_are_removed() {
        assert_eq!(
            user_words("a <b>bold</b> word <context>x</context> and <contextual>kept</contextual>"),
            "a <b>bold</b> word and <contextual>kept</contextual>"
        );
    }

    #[test]
    fn a_block_cut_off_before_its_closing_tag_is_removed_to_the_end() {
        assert_eq!(user_words("fix it <goal_context>\nRevision: 1"), "fix it");
    }
}
