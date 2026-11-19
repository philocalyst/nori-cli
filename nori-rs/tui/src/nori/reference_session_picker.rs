//! Past project conversations offered for insertion into a draft as a grounded reference. The
//! picker is [`crate::resume_picker`] with the reference purpose; this module loads its rows
//! and formats the chosen conversation.

use std::path::Path;

use nori_harness::transcript::TranscriptLoader;

use crate::resume_picker::SessionPickerInfo;

/// The sessions of this project, each with the agent that recorded it.
pub(crate) async fn load(
    nori_home: &Path,
    cwd: &Path,
) -> std::io::Result<Vec<(SessionPickerInfo, Option<String>)>> {
    let metadata = TranscriptLoader::new(nori_home.to_path_buf())
        .find_session_metadata_for_cwd(cwd)
        .await?;
    Ok(metadata
        .into_iter()
        .map(|item| {
            let agent = item.agent.clone();
            (SessionPickerInfo::from(item), agent)
        })
        .collect())
}

/// Include a bounded, explicitly labelled tail of actual user and assistant messages.
pub(crate) fn format_reference(
    project_id: &str,
    session_id: &str,
    agent: &str,
    transcript: &nori_harness::transcript::Transcript,
) -> String {
    let entries = crate::viewonly_transcript::transcript_to_entries(transcript);
    format_reference_entries(project_id, session_id, agent, &entries)
}

fn format_reference_entries(
    project_id: &str,
    session_id: &str,
    agent: &str,
    entries: &[crate::viewonly_transcript::ViewonlyEntry],
) -> String {
    use crate::viewonly_transcript::ViewonlyEntry;
    const MAX_CHARS: usize = 12_000;
    let mut remaining = MAX_CHARS;
    let mut parts = Vec::new();
    let mut truncated = false;
    for entry in entries.iter().rev() {
        let (role, content) = match entry {
            ViewonlyEntry::User { content } => ("User", content),
            ViewonlyEntry::Assistant { content } => ("Assistant", content),
            ViewonlyEntry::Thinking { .. } | ViewonlyEntry::Info { .. } => continue,
        };
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
        use crate::viewonly_transcript::ViewonlyEntry;
        let entries = vec![
            ViewonlyEntry::User {
                content: "earlier question".to_owned(),
            },
            ViewonlyEntry::Assistant {
                content: "é".repeat(12_005),
            },
        ];
        let text = format_reference_entries("p", "s", "codex", &entries);
        assert!(text.starts_with("@session[project=p, id=s, agent=codex]\n--- Conversation excerpt (earlier content omitted) ---\nAssistant: "));
        assert!(!text.contains("earlier question"));
        assert_eq!(text.matches('é').count(), 12_000);
        assert!(text.ends_with("--- End conversation excerpt ---\n"));
    }
}
