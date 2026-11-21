//! The `permission` event: an agent asked the person for permission.

use std::path::Path;

use globset::GlobSet;
use nori_config::AskForApproval;
use nori_pkl::kinds::GateEvent;
use nori_pkl::kinds::gate::PermissionDecision;
use nori_pkl::kinds::gate::Rule;
use nori_pkl::kinds::gate::ToolKind;
use nori_protocol::acp::v1 as acp;

use super::Gates;
use super::Hit;
use super::TextMatcher;
use super::call_facts::command_text;
use super::call_facts::project_path;
use super::call_facts::raw_paths;
use super::record::Decided;
use crate::backend::AcpBackend;

/// What a gate rule decides about a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Approve without asking the person.
    Allow,
    /// Refuse, telling the agent why.
    Deny { reason: String },
    /// Ask the person, whatever the approval policy says.
    Ask,
}

/// A compiled rule of a `permission` gate.
#[derive(Debug)]
pub(super) struct Permission {
    kinds: Vec<ToolKind>,
    title: Option<TextMatcher>,
    command: Option<TextMatcher>,
    paths: Option<GlobSet>,
    only_paths: Option<GlobSet>,
    pub(super) decision: Verdict,
}

/// What a request says about the call it is for: what the matchers read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionFacts {
    kind: ToolKind,
    title: String,
    command: Option<String>,
    /// Project-relative inside the project, absolute elsewhere.
    paths: Vec<String>,
}

impl PermissionFacts {
    /// Reads `request`; `cwd` is the project, which paths are made relative to.
    pub(crate) fn of(request: &acp::RequestPermissionRequest, cwd: &Path) -> Self {
        let fields = &request.tool_call.fields;
        let kind = match fields.kind {
            Some(acp::ToolKind::Read) => ToolKind::Read,
            Some(acp::ToolKind::Edit) => ToolKind::Edit,
            Some(acp::ToolKind::Delete) => ToolKind::Delete,
            Some(acp::ToolKind::Move) => ToolKind::Move,
            Some(acp::ToolKind::Search) => ToolKind::Search,
            Some(acp::ToolKind::Execute) => ToolKind::Execute,
            Some(acp::ToolKind::Think) => ToolKind::Think,
            Some(acp::ToolKind::Fetch) => ToolKind::Fetch,
            Some(acp::ToolKind::SwitchMode) => ToolKind::SwitchMode,
            Some(_) | None => ToolKind::Other,
        };
        let raw = fields.raw_input.as_ref();
        let mut paths: Vec<String> = fields
            .locations
            .iter()
            .flatten()
            .map(|location| project_path(&location.path, cwd))
            .chain(raw_paths(raw).map(|path| project_path(Path::new(path), cwd)))
            .collect();
        paths.sort_unstable();
        paths.dedup();
        Self {
            kind,
            title: fields.title.clone().unwrap_or_default(),
            command: command_text(raw),
            paths,
        }
    }

    /// What the call acts on, for the journal and the person's line.
    pub(crate) fn subject(&self) -> String {
        match (&self.command, self.title.trim(), self.paths.is_empty()) {
            (Some(command), _, _) => command.clone(),
            (None, title, _) if !title.is_empty() => title.to_owned(),
            (None, _, false) => self.paths.join(", "),
            (None, _, true) => "a tool request".to_owned(),
        }
    }
}

fn globs(globs: &[String]) -> Result<Option<GlobSet>, String> {
    if globs.is_empty() {
        return Ok(None);
    }
    nori_pkl::kinds::gate::glob_set(globs)
        .map(Some)
        .map_err(|error| error.to_string())
}

impl Permission {
    pub(super) fn compile(at: &str, rule: &Rule) -> Result<Self, String> {
        let Rule::Permission {
            kinds,
            title,
            command,
            paths,
            only_paths,
            decision,
            reason,
        } = rule
        else {
            return Err(format!("{at}: not a permission rule"));
        };
        let matcher = |field: &str, pattern: &Option<nori_pkl::kinds::gate::Pattern>| {
            pattern
                .as_ref()
                .map(|pattern| TextMatcher::compile(&format!("{at}.{field}"), pattern))
                .transpose()
        };
        Ok(Self {
            kinds: kinds.clone(),
            title: matcher("title", title)?,
            command: matcher("command", command)?,
            paths: globs(paths).map_err(|error| format!("{at}.paths: {error}"))?,
            only_paths: globs(only_paths).map_err(|error| format!("{at}.only_paths: {error}"))?,
            decision: match decision {
                PermissionDecision::Allow => Verdict::Allow,
                PermissionDecision::Deny => Verdict::Deny {
                    reason: reason.clone().unwrap_or_default(),
                },
                PermissionDecision::Ask => Verdict::Ask,
            },
        })
    }

    fn matches(&self, facts: &PermissionFacts) -> bool {
        (self.kinds.is_empty() || self.kinds.contains(&facts.kind))
            && self
                .title
                .as_ref()
                .is_none_or(|title| title.matches(&facts.title))
            && self.command.as_ref().is_none_or(|matcher| {
                facts
                    .command
                    .as_deref()
                    .is_some_and(|command| matcher.matches(command))
            })
            && self
                .paths
                .as_ref()
                .is_none_or(|set| facts.paths.iter().any(|path| set.is_match(path)))
            && self.only_paths.as_ref().is_none_or(|set| {
                !facts.paths.is_empty() && facts.paths.iter().all(|path| set.is_match(path))
            })
    }
}

impl Gates {
    /// The first rule of any `permission` gate that matches `facts`.
    pub(super) fn check_permission(&self, facts: &PermissionFacts) -> Option<Hit<'_, Permission>> {
        self.permission
            .iter()
            .find(|entry| entry.rule.matches(facts))
            .map(Hit::of)
    }
}

/// What the gates make of a permission request.
pub(in crate::backend) enum Gated {
    /// No gate has a say: the saved rules and the approval policy decide, as ever.
    Unmatched,
    /// A gate wants the person asked, whatever the approval policy says.
    Ask,
    /// A gate answers on the person's behalf.
    Answer {
        outcome: acp::RequestPermissionOutcome,
        /// What the agent may read beside the outcome: which gate, and why.
        meta: Option<acp::Meta>,
    },
}

fn option_of(
    options: &[acp::PermissionOption],
    kinds: [acp::PermissionOptionKind; 2],
) -> Option<&acp::PermissionOption> {
    kinds
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
}

impl AcpBackend {
    /// Consult the `permission` gates for `request`. `saved` is what a saved rule already answered:
    /// a refusal the person saved holds whatever a gate allows or asks.
    pub(in crate::backend) async fn gate_permission(
        &self,
        request: &acp::RequestPermissionRequest,
        policy: AskForApproval,
        saved: Option<&acp::RequestPermissionOutcome>,
    ) -> Gated {
        let gates = self.orchestrator.seed().tree.gates.current().await;
        if !gates.has_permission() {
            return Gated::Unmatched;
        }
        let facts = PermissionFacts::of(request, &self.cwd);
        let Some(hit) = gates.check_permission(&facts) else {
            return Gated::Unmatched;
        };
        let saved_refusal = saved.is_some_and(|outcome| match outcome {
            acp::RequestPermissionOutcome::Selected(selected) => request
                .options
                .iter()
                .find(|option| option.option_id == selected.option_id)
                .is_some_and(|option| {
                    matches!(
                        option.kind,
                        acp::PermissionOptionKind::RejectOnce
                            | acp::PermissionOptionKind::RejectAlways
                    )
                }),
            _ => false,
        });
        let subject = facts.subject();
        let line = |verb: &str| {
            format!(
                "gate {}: {verb} {}",
                hit.gate,
                super::record::brief(&subject)
            )
        };
        let decided = |decision: &'static str| {
            Decided::new(
                GateEvent::Permission,
                hit.gate,
                hit.index,
                decision,
                &subject,
            )
        };
        match &hit.rule.decision {
            Verdict::Deny { reason } => {
                let outcome = match option_of(
                    &request.options,
                    [
                        acp::PermissionOptionKind::RejectOnce,
                        acp::PermissionOptionKind::RejectAlways,
                    ],
                ) {
                    Some(option) => acp::RequestPermissionOutcome::Selected(
                        acp::SelectedPermissionOutcome::new(option.option_id.clone()),
                    ),
                    None => acp::RequestPermissionOutcome::Cancelled,
                };
                let mut meta = acp::Meta::new();
                meta.insert(
                    "nori".to_owned(),
                    serde_json::json!({
                        "gate": { "name": hit.gate, "rule": hit.index, "reason": reason }
                    }),
                );
                self.record_gate(decided("deny").detail(reason).line(line("denied")))
                    .await;
                Gated::Answer {
                    outcome,
                    meta: Some(meta),
                }
            }
            Verdict::Allow | Verdict::Ask if saved_refusal => Gated::Unmatched,
            Verdict::Allow => {
                let Some(option) = option_of(
                    &request.options,
                    [
                        acp::PermissionOptionKind::AllowOnce,
                        acp::PermissionOptionKind::AllowAlways,
                    ],
                ) else {
                    return Gated::Unmatched;
                };
                // Said only when the person would have been asked; under `never` they never are.
                let skipped = policy != AskForApproval::Never && saved.is_none();
                let allowed = decided("allow");
                self.record_gate(if skipped {
                    allowed.line(line("allowed"))
                } else {
                    allowed
                })
                .await;
                Gated::Answer {
                    outcome: acp::RequestPermissionOutcome::Selected(
                        acp::SelectedPermissionOutcome::new(option.option_id.clone()),
                    ),
                    meta: None,
                }
            }
            Verdict::Ask => {
                self.record_gate(decided("ask")).await;
                Gated::Ask
            }
        }
    }
}
