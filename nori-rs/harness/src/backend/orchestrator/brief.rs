//! What a child is told, and what the user sees a task called.

use super::delegation::Role;

/// The title shown for a task: the one given, else the task's first line, shortened.
pub(super) fn task_title(title: Option<&str>, task: &str) -> String {
    const MAX_CHARS: usize = 72;
    let source = title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| {
            task.lines()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("task")
        });
    if source.chars().count() <= MAX_CHARS {
        return source.to_owned();
    }
    let mut short: String = source.chars().take(MAX_CHARS - 1).collect();
    short.push('…');
    short
}

/// What the child is asked: the task, framed by its role and by what happens to its answer.
pub(super) fn child_prompt(role: Option<Role>, task: &str) -> String {
    let role = role.map(|role| match role {
        Role::Implementation => "implementation",
        Role::Research => "research",
        Role::Review => "review",
        Role::Design => "design",
        Role::Test => "test",
        Role::General => "general",
    });
    let mut prompt = String::new();
    if let Some(role) = role {
        prompt.push_str(&format!("Act as the {role} sub-agent for this task.\n"));
    }
    prompt.push_str(
        "Another agent delegated this task to you. Your final message is returned to it as your \
         result, so make it complete and self-contained.\n\n",
    );
    prompt.push_str(task.trim());
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn titles_come_from_the_title_or_the_first_line() {
        assert_eq!(task_title(Some("  Review  "), "x"), "Review");
        assert_eq!(
            task_title(None, "\n  Find the bug\nin parse.rs"),
            "Find the bug"
        );
        let long = "a".repeat(100);
        assert_eq!(task_title(None, &long).chars().count(), 72);
    }

    #[test]
    fn the_child_is_told_its_role_and_that_its_answer_is_returned() {
        let prompt = child_prompt(Some(Role::Review), "  Review parse.rs  ");
        assert!(prompt.starts_with("Act as the review sub-agent for this task.\n"));
        assert!(prompt.ends_with("\n\nReview parse.rs"));
    }
}
