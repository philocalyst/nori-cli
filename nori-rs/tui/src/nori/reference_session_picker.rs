//! Past project conversations offered for insertion into a draft as a grounded reference. The
//! picker is [`crate::resume_picker`] with the reference purpose; this module loads its rows
//! and formats the chosen conversation.

use std::path::Path;

use nori_harness::session_index::DigestTurn;
use nori_harness::session_index::TurnRole;

use crate::resume_picker::SessionPickerInfo;

/// The sessions of this project, each with the agent that recorded it, most recently active
/// first.
pub(crate) async fn load(nori_home: &Path, cwd: &Path) -> std::io::Result<Vec<SessionPickerInfo>> {
    Ok(crate::resume_picker::load::load(nori_home, Some(cwd), None)
        .await?
        .sessions)
}

/// Include a bounded, explicitly labelled tail of actual user and assistant messages.
pub(crate) fn format_reference(
    project_id: &str,
    session_id: &str,
    agent: &str,
    turns: &[DigestTurn],
) -> String {
    const MAX_CHARS: usize = 12_000;
    let mut remaining = MAX_CHARS;
    let mut parts = Vec::new();
    let mut truncated = false;
    for turn in turns.iter().rev() {
        let role = match turn.role {
            TurnRole::User => "User",
            TurnRole::Assistant => "Assistant",
            TurnRole::Tool => continue,
        };
        let content = &turn.text;
        if remaining == 0 {
            truncated = true;
            break;
        }
        let length = content.chars().count();
        let selected = if length > remaining {
            truncated = true;
            content.chars().skip(length - remaining).collect::<String>()
        } else {
            content.clone()
        };
        remaining = remaining.saturating_sub(selected.chars().count());
        parts.push(format!("{role}: {selected}"));
    }
    parts.reverse();
    let notice = if truncated {
        " (earlier content omitted)"
    } else {
        ""
    };
    let body = if parts.is_empty() {
        "No user or assistant text was recorded.".to_owned()
    } else {
        parts.join("\n\n")
    };
    format!(
        "@session[project={project_id}, id={session_id}, agent={agent}]\n--- Conversation excerpt{notice} ---\n{body}\n--- End conversation excerpt ---\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn reference_contains_bounded_actual_conversation_tail() {
        let turn = |role, text: String| DigestTurn {
            role,
            text,
            command: None,
            output: None,
        };
        let turns = vec![
            turn(TurnRole::User, "earlier question".to_owned()),
            turn(TurnRole::Assistant, "é".repeat(12_005)),
        ];
        let text = format_reference("p", "s", "codex", &turns);
        assert!(text.starts_with("@session[project=p, id=s, agent=codex]\n--- Conversation excerpt (earlier content omitted) ---\nAssistant: "));
        assert!(!text.contains("earlier question"));
        assert_eq!(text.matches('é').count(), 12_000);
        assert!(text.ends_with("--- End conversation excerpt ---\n"));
    }
}
