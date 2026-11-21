//! What Nori says along with a prompt, by kind, and the one place that puts it on the wire.
//!
//! The words a person typed reach the agent as they were: their blocks are an ordered, byte-exact
//! subsequence of the wire prompt. Everything else Nori adds is an assistant-only block, in an
//! order that a provider's prompt cache can use. The cache matches exact prefixes, so what
//! changes least comes first and what is the same for every session that starts alike (the
//! session preamble, the agent catalog, the workflow rules) sits right after the agent's own
//! system prompt:
//!
//! 1. the static block: session preamble, agent catalog, workflow rules;
//! 2. the dynamic blocks: the checkpoint a compaction, fork or resume left (and the typed
//!    history that came with it), then the output of the session's hooks;
//! 3. the person's content;
//! 4. the goal context, which follows the words it is about.
//!
//! [`PromptContext::assemble`] is the only function that puts these together; nothing else
//! concatenates injected text. A queued prompt carries only what belongs to it (its workflow
//! rules). The parts the session owes whichever prompt is sent next are taken when the prompt is
//! sent and given back if it never reaches the agent: see [`session::SessionStores`].

pub(super) mod session;

use nori_protocol::acp::v1 as acp;

use super::thread_goal::GoalDelivery;

/// The words that open every session alike, so that a provider can reuse the cached prefix.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticContext {
    /// The session preamble (`tui/session_context*.md`).
    pub preamble: Option<String>,
    /// The cached agent catalog (`orchestrator/capabilities.rs`).
    pub catalog: Option<String>,
    /// The rules of the active workflow, until the agent has been told them.
    pub workflow_rules: Option<String>,
}

/// What a session has to say that depends on what happened to it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicContext {
    /// The checkpoint a compaction, fork or resume left, with its typed history still in it.
    pub checkpoint: Option<String>,
    /// What the session's hooks said to put before the agent (`::context::` lines).
    pub hook_output: Option<String>,
}

/// The goal context a prompt carries, and what to forget if the prompt never goes out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalContext {
    pub text: String,
    /// What the conversation had been told before this was taken.
    pub before: GoalDelivery,
}

/// Everything Nori adds around one prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptContext {
    pub fixed: StaticContext,
    pub dynamic: DynamicContext,
    pub goal: Option<GoalContext>,
}

fn assistant_only(text: String) -> Option<acp::ContentBlock> {
    (!text.is_empty()).then(|| {
        acp::ContentBlock::Text(
            acp::TextContent::new(text)
                .annotations(acp::Annotations::new().audience(vec![acp::Role::Assistant])),
        )
    })
}

/// Whether `block` is something Nori added around a prompt (see [`PromptContext::assemble`]),
/// as an agent that echoes the prompt back sends it.
pub(crate) fn is_injected(block: &acp::ContentBlock) -> bool {
    matches!(block, acp::ContentBlock::Text(text)
        if text
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.audience.as_deref())
            == Some(&[acp::Role::Assistant][..]))
}

impl PromptContext {
    /// The prompt as it goes to the agent: this context around `person`, which stays as it is.
    pub(crate) fn assemble(&self, person: Vec<acp::ContentBlock>) -> Vec<acp::ContentBlock> {
        let mut wire = Vec::new();
        let fixed = [
            &self.fixed.preamble,
            &self.fixed.catalog,
            &self.fixed.workflow_rules,
        ]
        .into_iter()
        .flatten()
        .filter(|text| !text.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
        wire.extend(assistant_only(fixed));
        if let Some(checkpoint) = &self.dynamic.checkpoint {
            let mut checkpoint = checkpoint.clone();
            let historical = crate::transcript::fork_context::take_typed_history(&mut checkpoint);
            wire.extend(assistant_only(checkpoint));
            wire.extend(historical);
        }
        wire.extend(self.dynamic.hook_output.clone().and_then(assistant_only));
        wire.extend(person);
        wire.extend(
            self.goal
                .as_ref()
                .and_then(|goal| assistant_only(goal.text.clone())),
        );
        wire
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn text(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(acp::TextContent::new(text))
    }

    fn injected(text: &str) -> acp::ContentBlock {
        acp::ContentBlock::Text(
            acp::TextContent::new(text)
                .annotations(acp::Annotations::new().audience(vec![acp::Role::Assistant])),
        )
    }

    fn person() -> Vec<acp::ContentBlock> {
        vec![
            text("  meaningful work\n"),
            acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2U=", "image/png")),
            text("second block\n\n"),
        ]
    }

    fn full_context() -> PromptContext {
        PromptContext {
            fixed: StaticContext {
                preamble: Some("<context>preamble</context>".into()),
                catalog: Some("<nori_agent_catalog>agents</nori_agent_catalog>".into()),
                workflow_rules: Some("<workflow>\nrules\n</workflow>".into()),
            },
            dynamic: DynamicContext {
                checkpoint: Some(
                    "<conversation-checkpoint>\n<summary>\nwhat happened\n</summary>\n</conversation-checkpoint>"
                        .into(),
                ),
                hook_output: Some("from a hook".into()),
            },
            goal: Some(GoalContext {
                text: "<goal_context>\nship\n</goal_context>".into(),
                before: GoalDelivery::default(),
            }),
        }
    }

    #[test]
    fn the_static_header_comes_first_then_the_dynamic_blocks_then_the_person_then_the_goal() {
        assert_eq!(
            full_context().assemble(person()),
            vec![
                injected(
                    "<context>preamble</context>\n\n<nori_agent_catalog>agents</nori_agent_catalog>\n\n<workflow>\nrules\n</workflow>"
                ),
                injected(
                    "<conversation-checkpoint>\n<summary>\nwhat happened\n</summary>\n</conversation-checkpoint>"
                ),
                injected("from a hook"),
                text("  meaningful work\n"),
                acp::ContentBlock::Image(acp::ImageContent::new("aW1hZ2U=", "image/png")),
                text("second block\n\n"),
                injected("<goal_context>\nship\n</goal_context>"),
            ]
        );
    }

    #[test]
    fn the_static_header_is_the_same_bytes_whatever_the_session_adds_after_it() {
        let alike = PromptContext {
            dynamic: DynamicContext::default(),
            goal: None,
            ..full_context()
        };
        assert_eq!(
            full_context().assemble(person())[0],
            alike.assemble(person())[0]
        );
    }

    #[test]
    fn typed_history_follows_its_checkpoint() {
        let history = vec![text("archived evidence")];
        let encoded = serde_json::to_string(&history).unwrap();
        let context = PromptContext {
            dynamic: DynamicContext {
                checkpoint: Some(format!(
                    "<conversation-checkpoint>\n<summary>\nsummary\n<nori-typed-history>{encoded}</nori-typed-history>\n</summary>\n</conversation-checkpoint>"
                )),
                hook_output: None,
            },
            ..PromptContext::default()
        };
        assert_eq!(
            context.assemble(person()),
            [
                vec![
                    injected(
                        "<conversation-checkpoint>\n<summary>\nsummary\n</summary>\n</conversation-checkpoint>"
                    ),
                    text("archived evidence"),
                ],
                person(),
            ]
            .concat()
        );
    }

    #[test]
    fn injected_context_is_assistant_only_and_human_blocks_stay_byte_exact() {
        let wire = full_context().assemble(person());
        let human = wire
            .iter()
            .filter(|block| {
                !matches!(block, acp::ContentBlock::Text(text)
                    if text
                        .annotations
                        .as_ref()
                        .and_then(|annotations| annotations.audience.as_ref())
                        .is_some())
            })
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(human, person());
        assert_eq!(wire.len(), person().len() + 4);
    }

    #[test]
    fn every_tagged_block_is_removed_from_the_words_a_person_asked() {
        let wire = full_context().assemble(vec![text("  fix the build\n")]);
        let wire_text = wire
            .iter()
            .filter_map(|block| match block {
                acp::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // A hook's output is whatever the hook printed, so it carries no tag to remove.
        let wire_text = wire_text.replace("from a hook", "");
        assert_eq!(
            crate::injected_context::user_words(&wire_text),
            "fix the build"
        );
    }

    #[test]
    fn no_context_adds_no_synthetic_block() {
        let empty = PromptContext {
            fixed: StaticContext {
                preamble: Some(String::new()),
                ..StaticContext::default()
            },
            dynamic: DynamicContext {
                hook_output: Some(String::new()),
                ..DynamicContext::default()
            },
            goal: None,
        };
        assert_eq!(empty.assemble(person()), person());
        assert_eq!(PromptContext::default().assemble(person()), person());
    }
}
