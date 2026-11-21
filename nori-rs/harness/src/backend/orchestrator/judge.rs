//! One-shot reviewers: a fresh actor that is asked one question, has no Nori tools, and whose
//! final message is the answer.
//!
//! This is `delegate_task` with the person as the requester (nothing is owed to any model, so no
//! completion wake is ever delivered) and an empty tool selection, waited on in the harness's own
//! task. The goal judge (`crate::backend::goal_judge`) is the first user.

use std::sync::Arc;
use std::time::Duration;

use super::Orchestrator;
use super::delegation::Delegation;
use super::program::ActorScope;
use super::status::Waiter;
use super::task::Party;

/// How often a reviewer that waits on the person's approval is looked at again.
const APPROVAL_RECHECK: Duration = Duration::from_secs(1);

/// What a reviewer is asked, and on what.
pub(crate) struct ReviewRequest {
    /// The agent it runs on, as in Nori's agent configuration.
    pub(crate) agent: String,
    /// Its model; `None` leaves the agent's default.
    pub(crate) model: Option<String>,
    /// What the person sees the reviewer called.
    pub(crate) title: String,
    pub(crate) prompt: String,
    /// All it may take, from starting its session to its answer.
    pub(crate) timeout: Duration,
}

/// Why a reviewer gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReviewFailure {
    /// It could not be started, or its session failed or was cancelled.
    NotRun { cause: String },
    /// It did not answer in time, and was stopped.
    TimedOut,
}

impl Orchestrator {
    /// Run a reviewer with no Nori tools on `request.prompt`, and return its final message. It
    /// starts with fresh context, so it sees only the prompt.
    pub(crate) async fn review(&self, request: ReviewRequest) -> Result<String, ReviewFailure> {
        let inner = Arc::clone(self.inner());
        let deadline = tokio::time::Instant::now() + request.timeout;
        let delegation = Delegation {
            agent: Some(request.agent),
            model: request.model,
            title: Some(request.title),
            requester: Some(Party::User),
            // An empty selection: the reviewer is given none of Nori's tools, so it cannot call
            // `update_goal` or delegate in turn.
            scope: Some(Arc::new(ActorScope {
                tools: Some(Vec::new()),
                ..ActorScope::default()
            })),
            ..Delegation::new(request.prompt)
        };
        let started = tokio::time::timeout_at(deadline, inner.delegate(delegation))
            .await
            .map_err(|_| ReviewFailure::TimedOut)?
            .map_err(|cause| ReviewFailure::NotRun { cause })?;
        let task = started.task_id.ok_or_else(|| ReviewFailure::NotRun {
            cause: "the reviewer was started without a task".to_owned(),
        })?;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                inner.cancel_task(&task).await;
                return Err(ReviewFailure::TimedOut);
            }
            // One wait is capped at what a tool call may block; a longer budget is several waits.
            let waited = inner
                .wait_on(std::slice::from_ref(&task), remaining, Waiter::Program)
                .await
                .map_err(|cause| ReviewFailure::NotRun { cause })?;
            let Some(snapshot) = waited.tasks.into_iter().next() else {
                return Err(ReviewFailure::NotRun {
                    cause: "the reviewer's task disappeared".to_owned(),
                });
            };
            if snapshot.in_flight() {
                // A wait returns at once for a reviewer asking the person's approval; look again
                // in a moment instead of spinning until the person answers or the time is up.
                if snapshot.awaiting_approval == Some(true) {
                    tokio::time::sleep(APPROVAL_RECHECK.min(remaining)).await;
                }
                continue;
            }
            return match (snapshot.result, snapshot.error) {
                (Some(result), _) if !result.trim().is_empty() => Ok(result),
                (_, Some(error)) => Err(ReviewFailure::NotRun { cause: error }),
                (_, None) => Err(ReviewFailure::NotRun {
                    cause: format!("the reviewer ended {} without an answer", snapshot.status),
                }),
            };
        }
    }
}
