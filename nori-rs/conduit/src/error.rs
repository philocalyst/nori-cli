//! Error types for `nori-conduit`.

use nori_workflow::FailureKind;
use serde::Deserialize;
use serde::Serialize;

/// One thing wrong with a program, located in its source when that can be told.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Problem {
    /// `file.pkl:12`, or the step path (`steps[2].branches[0]`) when no line is known.
    pub location: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConduitError {
    #[error("Pkl evaluation failed: {message}")]
    Compilation { message: String },

    #[error("{}", render_problems(.file, .problems))]
    InvalidProgram {
        file: String,
        problems: Vec<Problem>,
    },

    #[error("invalid Pkl diff: {message}")]
    PklDiff { message: String },

    #[error("optimistic concurrency conflict: expected base_rev {expected}, actual rev {actual}")]
    RevisionConflict { expected: i64, actual: i64 },

    #[error("command_id `{command_id}` was already committed with a different payload")]
    DuplicateCommandId { command_id: String },

    #[error("sandbox policy violation: {reason}")]
    SandboxViolation { reason: String },

    #[error("token budget exceeded: limit {limit}, spent {spent}, requested {requested}")]
    BudgetExceeded {
        limit: i64,
        spent: i64,
        requested: i64,
    },

    #[error("guard ({phase}) rejected after {attempts} attempt(s): {feedback}")]
    GuardRejected {
        phase: String,
        feedback: String,
        attempts: i64,
    },

    #[error("loop did not reach its `until` condition within {max_iterations} iteration(s)")]
    LoopExhausted { max_iterations: i64 },

    #[error("step timed out after {ms}ms")]
    Timeout { ms: i64 },

    #[error("all branches failed")]
    AllBranchesFailed,

    #[error("`{{{{{name}}}}}` is not bound at this point; bound names: {bound}")]
    UnboundName { name: String, bound: String },

    #[error("unknown actor `{alias}`; addressable actors: {known}")]
    UnknownActor { alias: String, known: String },

    #[error("unknown tool `{name}`")]
    UnknownTool { name: String },

    #[error("unknown or already resumed interrupt `{id}`")]
    UnknownInterrupt { id: String },

    #[error("tool `{name}` failed: {reason}")]
    ToolFailure { name: String, reason: String },

    #[error("child agent `{agent_id}` failed: {reason}")]
    ChildFailed { agent_id: String, reason: String },

    /// A request to an actor's model failed. `kind` drives retry and fallback.
    #[error("{} ({}): {message}", .class.label(), .class.name())]
    Host {
        class: FailureKind,
        message: String,
        retry_after_ms: Option<i64>,
    },

    /// This host or build cannot do what the program asked, and says so.
    #[error("{what} is not available: {reason}")]
    Unsupported { what: String, reason: String },

    #[error("the run was cancelled")]
    Cancelled,

    #[error("run journal failed: {message}")]
    Journal { message: String },

    #[error("runtime error: {message}")]
    Runtime { message: String },

    #[error("wire protocol error: {message}")]
    Wire { message: String },
}

fn render_problems(file: &str, problems: &[Problem]) -> String {
    let mut text = format!(
        "{file} is not a valid program ({} problem{}):",
        problems.len(),
        if problems.len() == 1 { "" } else { "s" }
    );
    for problem in problems {
        text.push_str(&format!("\n  {}: {}", problem.location, problem.message));
    }
    text
}

impl ConduitError {
    /// How recovery classifies this failure. Failures that no retry or fallback can change
    /// (policy violations, validation, cancellation) are `None`.
    pub fn failure_kind(&self) -> Option<FailureKind> {
        match self {
            Self::Host { class, .. } => Some(*class),
            Self::ChildFailed { .. }
            | Self::ToolFailure { .. }
            | Self::GuardRejected { .. }
            | Self::LoopExhausted { .. }
            | Self::Timeout { .. }
            | Self::AllBranchesFailed
            | Self::Runtime { .. } => Some(FailureKind::Unknown),
            Self::Compilation { .. }
            | Self::InvalidProgram { .. }
            | Self::PklDiff { .. }
            | Self::RevisionConflict { .. }
            | Self::DuplicateCommandId { .. }
            | Self::SandboxViolation { .. }
            | Self::BudgetExceeded { .. }
            | Self::UnboundName { .. }
            | Self::UnknownActor { .. }
            | Self::UnknownTool { .. }
            | Self::UnknownInterrupt { .. }
            | Self::Unsupported { .. }
            | Self::Cancelled
            | Self::Journal { .. }
            | Self::Wire { .. } => None,
        }
    }
}
