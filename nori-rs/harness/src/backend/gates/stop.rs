//! The `stop` event: a local turn ended with `end_turn`.

use std::collections::HashMap;

use globset::GlobSet;
use nori_pkl::kinds::GateEvent;
use nori_pkl::kinds::gate::Rule;
use nori_pkl::kinds::gate::StopDecision;
use nori_protocol::acp::v1 as acp;

use super::Gates;
use super::Hit;
use super::TextMatcher;
use super::call_facts::project_path;
use super::record::Decided;
use super::record::brief;
use crate::backend::AcpBackend;
use crate::backend::session_reducer::CompletedTurn;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

/// What a `stop` rule does.
#[derive(Debug)]
pub(super) enum Decision {
    Continue { message: String },
    Pass,
}

/// A compiled rule of a `stop` gate.
#[derive(Debug)]
pub(super) struct Stop {
    last_message: Option<TextMatcher>,
    changed: Option<GlobSet>,
    pub(super) decision: Decision,
    max_continues: i64,
}

/// What a finished turn says: what the matchers read.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StopFacts<'a> {
    /// The agent's last message of the turn; empty when it said nothing.
    pub(crate) last_message: &'a str,
    /// The files the turn changed, project-relative.
    pub(crate) changed: &'a [String],
}

/// How many times each continuing rule has continued the turns of the current prompt of the
/// person, by (gate, rule).
#[derive(Debug, Default)]
pub(crate) struct ContinueCounts(HashMap<(String, i64), i64>);

impl ContinueCounts {
    /// A prompt of the person begins a new task: every rule may continue again.
    pub(crate) fn clear(&mut self) {
        self.0.clear();
    }

    fn used(&self, gate: &str, rule: i64) -> i64 {
        self.0
            .get(&(gate.to_owned(), rule))
            .copied()
            .unwrap_or_default()
    }

    fn add(&mut self, gate: &str, rule: i64, by: i64) {
        *self.0.entry((gate.to_owned(), rule)).or_default() += by;
    }

    /// Gives back a continuation that could not be delivered.
    pub(crate) fn refund(&mut self, gate: &str, rule: i64) {
        self.add(gate, rule, -1);
    }
}

impl Stop {
    pub(super) fn compile(at: &str, rule: &Rule) -> Result<Self, String> {
        let Rule::Stop {
            last_message,
            changed,
            decision,
            message,
            max_continues,
        } = rule
        else {
            return Err(format!("{at}: not a stop rule"));
        };
        Ok(Self {
            last_message: last_message
                .as_ref()
                .map(|pattern| TextMatcher::compile(&format!("{at}.last_message"), pattern))
                .transpose()?,
            changed: if changed.is_empty() {
                None
            } else {
                Some(
                    nori_pkl::kinds::gate::glob_set(changed)
                        .map_err(|error| format!("{at}.changed: {error}"))?,
                )
            },
            decision: match decision {
                StopDecision::Continue => Decision::Continue {
                    message: message.clone().unwrap_or_default(),
                },
                StopDecision::Pass => Decision::Pass,
            },
            max_continues: *max_continues,
        })
    }

    fn matches(&self, facts: &StopFacts<'_>) -> bool {
        self.last_message
            .as_ref()
            .is_none_or(|matcher| matcher.matches(facts.last_message))
            && self
                .changed
                .as_ref()
                .is_none_or(|set| facts.changed.iter().any(|path| set.is_match(path)))
    }
}

impl Gates {
    /// The first rule of any `stop` gate that matches `facts`. A rule that continues and has
    /// already continued as often as it may in this task is spent and skipped; a continuation it
    /// returns is counted.
    pub(super) fn check_stop(
        &self,
        facts: &StopFacts<'_>,
        counts: &mut ContinueCounts,
    ) -> Option<Hit<'_, Stop>> {
        for entry in &self.stop {
            if !entry.rule.matches(facts) {
                continue;
            }
            if matches!(entry.rule.decision, Decision::Continue { .. }) {
                if counts.used(&entry.gate, entry.index) >= entry.rule.max_continues {
                    continue;
                }
                counts.add(&entry.gate, entry.index, 1);
            }
            return Some(Hit::of(entry));
        }
        None
    }
}

impl AcpBackend {
    /// Consult the `stop` gates about the turn that just ended, and continue it when one says so.
    /// Only the person's own session is gated: a delegated child ending its turn is the
    /// `subagent_stop` event, which is designed but not built.
    pub(in crate::backend) async fn gate_stop(&self, completed: &CompletedTurn, generation: i64) {
        if completed.stop_reason != acp::StopReason::EndTurn || self.orchestrator.is_child() {
            return;
        }
        let cell = &self.orchestrator.seed().tree.gates;
        if completed.prompt.kind == QueuedPromptKind::User {
            cell.with_continues(ContinueCounts::clear);
        }
        let gates = cell.current().await;
        if !gates.has_stop() {
            return;
        }
        let changed: Vec<String> = self
            .edit_journal
            .latest_turn_changed_paths()
            .iter()
            .map(|path| project_path(path, &self.cwd))
            .collect();
        let last_message = completed.last_agent_message.as_deref().unwrap_or_default();
        let facts = StopFacts {
            last_message,
            changed: &changed,
        };
        let Some(hit) = cell.with_continues(|counts| gates.check_stop(&facts, counts)) else {
            return;
        };
        let subject = brief(last_message);
        let decided = |decision: &'static str| {
            Decided::new(GateEvent::Stop, hit.gate, hit.index, decision, &subject)
        };
        let Decision::Continue { message } = &hit.rule.decision else {
            self.record_gate(decided("pass")).await;
            return;
        };
        let text = format!(
            "<gate_continue gate=\"{}\">\n{message}\n</gate_continue>",
            hit.gate
        );
        let started = self
            .offer_prompt(
                Some(generation),
                QueuedPrompt {
                    event_id: format!("gate-continue-{}", uuid::Uuid::new_v4()),
                    kind: QueuedPromptKind::GateContinue,
                    text: text.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                    context: Default::default(),
                    display_text: None,
                },
            )
            .await;
        if !started {
            cell.with_continues(|counts| counts.refund(hit.gate, hit.index));
            return;
        }
        let line = format!("gate {}: continued the turn ({})", hit.gate, brief(message));
        self.record_gate(decided("continue").detail(message).line(line))
            .await;
    }
}
