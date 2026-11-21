//! Codex's approval presets as engine modes, and its slash commands as prompt templates.
//!
//! Codex backs its presets with an OS sandbox; the engine has none, so each preset is the rules
//! that come closest without one:
//!
//! - `read-only`: edits are refused; commands ask, except those known only to read.
//! - `auto` (Codex's "Default"): edits inside the workspace run; commands ask, except those known
//!   only to read; anything outside the workspace asks.
//! - `full-access`: everything runs without asking. Nori's default for Codex, as before: Nori, not
//!   Codex, owns approvals, and the person picks a stricter preset per session.

use nori_engine::CommandDef;
use nori_engine::Effect;
use nori_engine::Mode;
use nori_engine::READ_ONLY_SHELL;
use nori_engine::Rule;

pub(crate) const DEFAULT_MODE: &str = "full-access";

const READ_ONLY_PROMPT: &str = "You are in read-only mode: file edits are refused, and commands that may change anything need the user's approval. Investigate, explain and propose changes; do not try to work around the restriction.";

pub(crate) fn modes() -> Vec<Mode> {
    let reads = Rule::new(READ_ONLY_SHELL, "*", Effect::Allow);
    vec![
        Mode {
            id: "read-only".to_owned(),
            label: "Read Only".to_owned(),
            description: Some(
                "Codex can read files in the current workspace. Approval is required to edit files or run commands that may change anything."
                    .to_owned(),
            ),
            rules: vec![
                Rule::new("edit", "*", Effect::Deny),
                Rule::new("shell", "*", Effect::Ask),
                reads.clone(),
            ],
            prompt: Some(READ_ONLY_PROMPT.to_owned()),
            model: None,
        },
        Mode {
            id: "auto".to_owned(),
            label: "Default".to_owned(),
            description: Some(
                "Codex can read and edit files in the current workspace. Approval is required to run commands that may change anything or to touch other files."
                    .to_owned(),
            ),
            rules: vec![
                Rule::new("edit", "*", Effect::Allow),
                Rule::new("shell", "*", Effect::Ask),
                reads,
            ],
            prompt: None,
            model: None,
        },
        Mode {
            id: DEFAULT_MODE.to_owned(),
            label: "Full Access".to_owned(),
            description: Some(
                "Codex can edit files outside this workspace and run any command without asking for approval. Exercise caution when using."
                    .to_owned(),
            ),
            rules: vec![Rule::new("*", "*", Effect::Allow)],
            prompt: None,
            model: None,
        },
    ]
}

/// The base rules every preset starts from: work outside the workspace asks.
pub(crate) fn base_rules() -> Vec<Rule> {
    vec![Rule::new("external_directory", "*", Effect::Ask)]
}

pub(crate) fn commands() -> Vec<CommandDef> {
    vec![
        CommandDef {
            name: "init".to_owned(),
            description: "create an AGENTS.md file with instructions for Codex".to_owned(),
            template: include_str!("../prompt_for_init_command.md").to_owned(),
        },
        CommandDef {
            name: "review".to_owned(),
            description: "Review my current changes and find issues".to_owned(),
            template: format!(
                "{}\n\n---\n\nReview the current code changes (staged, unstaged, and untracked files) and provide prioritized findings.\n\n$ARGUMENTS",
                codex_prompts::REVIEW_PROMPT
            ),
        },
    ]
}
