//! Events about where a session runs: it moved to another route, or is about to retry.
//!
//! Policy vocabulary ([`FailureKind`], [`Route`], [`RouteRef`]) lives in `nori-workflow`; these
//! are the facts the harness reports when it applies that policy.

use serde::Deserialize;
use serde::Serialize;

pub use nori_workflow::FailureKind;
pub use nori_workflow::ReturnToPrimary;
pub use nori_workflow::Route;
pub use nori_workflow::RouteCause;
pub use nori_workflow::RouteRef;

/// Where a fallback stands: step `index` of `total` routes the harness planned (1-based).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackStep {
    pub index: i64,
    pub total: i64,
}

/// A request failed in a way the policy retries: the next attempt on the same route starts
/// `wait_ms` from now, unless the user cancels first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryScheduled {
    pub class: FailureKind,
    /// The retry about to happen, counting from 1.
    pub attempt: i64,
    pub max_attempts: i64,
    pub wait_ms: i64,
    /// What the failing route said, as the agent put it.
    pub detail: Option<String>,
}

/// What made a failed attempt's output obsolete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupersededBy {
    /// The same request was sent again on the same route.
    Retry,
    /// The request moves to another route.
    Route,
}

/// An attempt at the current request failed and another follows: whatever it streamed so far
/// (partial answer, tool calls) is the failed attempt's, and the new attempt starts over. The
/// output stays in the transcript; this is what lets a front end mark it as superseded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptSuperseded {
    /// The attempt that failed, counting from 1.
    pub attempt: i64,
    pub by: SupersededBy,
}

/// A session moved from one route to another: its route failed, or the route the user chose is
/// usable again. The session may now run on another agent. It never happens silently: the
/// session's header always shows the route that is actually running.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteChanged {
    pub from: RouteRef,
    pub to: RouteRef,
    pub cause: RouteCause,
    pub step: FallbackStep,
    /// What the failing route said, as the agent put it.
    pub detail: Option<String>,
    /// The account each route spends from (`Claude · primary`), when they differ in more than
    /// the agent: a move from `claude-code` to `claude2` is a move between two accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_account: Option<String>,
}
