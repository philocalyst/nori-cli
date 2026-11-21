//! A request, from a session's backend to the runtime that owns it, to continue the
//! conversation on another agent.
//!
//! One agent's session cannot start another agent, so the backend that decides on a route on a
//! different agent sends this and the runtime (`runtime/handoff.rs`) replaces the backend,
//! carrying the conversation context and, when there is one, the prompt to send again.

use nori_protocol::acp::v1 as acp;
use nori_workflow::Route;
use nori_workflow::RouteCause;
use nori_workflow::RouteRef;

/// The prompt to send again on the new route.
pub(crate) struct HandoffPrompt {
    pub(crate) content: Vec<acp::ContentBlock>,
    /// The prompt's metadata, which keeps its echo identity so the transcript shows it once.
    pub(crate) meta: Option<acp::Meta>,
    /// The prompt's request ID on the public stream. The new route answers under it, so a client
    /// waiting on the request it sent sees it end, wherever it ended.
    pub(crate) request_id: acp::RequestId,
}

pub(crate) struct HandoffRequest {
    pub(crate) from: RouteRef,
    pub(crate) cause: RouteCause,
    /// Candidate routes, best first; the first that starts wins.
    pub(crate) plan: Vec<Route>,
    pub(crate) prompt: Option<HandoffPrompt>,
    /// What the failing route said, when it failed.
    pub(crate) detail: Option<String>,
}
