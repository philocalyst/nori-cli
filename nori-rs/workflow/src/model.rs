//! Plain data produced by a workflow script.
//!
//! Nothing here knows about Scheme: the harness applies these values directly.

use crate::compaction::CompactionPolicy;
use crate::goals::GoalsPolicy;
use crate::loops::LoopPolicy;
use crate::recovery::RecoveryPolicy;

/// How an agent session should behave: its autonomy, the prompts Nori sends at
/// session events, and the commit and pull-request rules the agent follows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Workflow {
    pub name: String,
    /// How many prompt turns the agent may run on its own before control
    /// returns to the user. `None` leaves the harness default in place.
    pub max_turns: Option<i64>,
    /// Hooks in declaration order. Several hooks may share an event.
    pub hooks: Vec<Hook>,
    pub commit: Option<CommitRules>,
    /// Agent that reviews changed work and proposes commit messages without committing.
    pub commit_reviewer: Option<CommitAgent>,
    /// How a failed request is retried and which routes a session falls back to.
    pub recovery: RecoveryPolicy,
    /// When a session compacts itself, and how a refused checkpoint is cut down.
    pub compaction: CompactionPolicy,
    /// What happens when the agent keeps repeating one tool call.
    pub loops: LoopPolicy,
    /// Whether an independent reviewer checks a goal the agent calls complete.
    pub goals: GoalsPolicy,
    pub pr: Option<PrRules>,
    /// Free-form standing instructions, rendered verbatim.
    pub instructions: Vec<String>,
    /// Optional session-local selection of Nori-owned host tools.
    pub tools: Option<ToolPolicy>,
}

impl Workflow {
    /// A workflow with only a name.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// The prompts to send when `event` happens, in declaration order.
    pub fn hook_prompts(&self, event: HookEvent) -> impl Iterator<Item = &str> {
        self.hooks
            .iter()
            .filter(move |hook| hook.event == event)
            .map(|hook| hook.prompt.as_str())
    }
}

/// A prompt Nori sends to the agent when a session event happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hook {
    pub event: HookEvent,
    pub prompt: String,
}

/// The session events a [`Hook`] can follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    /// A prompt turn ended having changed files in the working tree.
    AfterChange,
    /// Any prompt turn ended.
    AfterTurn,
    /// The agent created a commit.
    AfterCommit,
    /// The agent is about to open or update a pull request.
    BeforePr,
    /// The agent is about to hand control back to the user.
    BeforeStop,
    /// A check failed on a commit the agent pushed.
    CiFailed,
    /// Every check passed on a commit the agent pushed.
    CiPassed,
}

impl HookEvent {
    pub const ALL: [HookEvent; 7] = [
        HookEvent::AfterChange,
        HookEvent::AfterTurn,
        HookEvent::AfterCommit,
        HookEvent::BeforePr,
        HookEvent::BeforeStop,
        HookEvent::CiFailed,
        HookEvent::CiPassed,
    ];

    /// The DSL form that declares a hook for this event.
    pub fn form(self) -> &'static str {
        match self {
            HookEvent::AfterChange => "on-change",
            HookEvent::AfterTurn => "on-turn",
            HookEvent::AfterCommit => "on-commit",
            HookEvent::BeforePr => "on-pr",
            HookEvent::BeforeStop => "on-stop",
            HookEvent::CiFailed => "on-ci-fail",
            HookEvent::CiPassed => "on-ci-pass",
        }
    }
}

/// What a commit looks like and when commits are made.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommitRules {
    /// `None` until a `commit-style` form sets it.
    pub style: Option<CommitStyle>,
    pub max_files: Option<i64>,
    /// Longest allowed subject line, in characters.
    pub max_subject: Option<i64>,
    /// Each commit is one logical change that builds on its own.
    pub atomic: bool,
    pub batch: Option<CommitBatch>,
    /// Every commit message has a body explaining why.
    pub require_body: bool,
    /// When set, this agent writes the commits instead of the session agent.
    pub agent: Option<CommitAgent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitStyle {
    /// `type(scope): summary`, per conventionalcommits.org.
    Conventional,
    /// An imperative summary line with no prefix.
    Plain,
}

/// When accumulated changes are turned into commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitBatch {
    /// Commit as soon as each logical change is complete.
    PerChange,
    /// Commit at the end of every prompt turn that changed files.
    PerTurn,
    /// Commit once, when the task is complete.
    PerTask,
}

/// The agent and model designated to write and batch commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAgent {
    /// The agent's slug, as in Nori's agent configuration (`claude`, `codex`, ...).
    pub agent: String,
    pub model: Option<String>,
}

/// What a pull request must contain and when work is upstreamed.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrRules {
    /// A title template such as `<type>(<scope>): <summary>`.
    pub title: Option<String>,
    /// Required description sections, in order.
    pub sections: Vec<String>,
    /// Largest diff, in changed lines, before the work must be split.
    pub max_lines: Option<i64>,
    pub max_files: Option<i64>,
    /// The branch pull requests target.
    pub base: Option<String>,
    pub draft: bool,
    pub upstream: Option<Upstream>,
}

/// When changes leave the machine: pushing a branch and opening a pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upstream {
    /// Never push; the user upstreams by hand.
    Never,
    /// Ask the user before pushing or opening a pull request.
    Ask,
    /// Push and open the pull request once the tests pass.
    TestsPass,
    /// Push and open the pull request when the task is complete.
    Done,
}

/// Selection and model-facing instructions for Nori-owned tools.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolPolicy {
    pub enabled: Option<Vec<String>>,
    pub disabled: Vec<String>,
    pub descriptions: std::collections::BTreeMap<String, String>,
    pub instructions: Vec<String>,
}
