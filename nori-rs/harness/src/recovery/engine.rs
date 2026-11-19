//! The decision a failed request gets: retry it, move to another route, or give up.
//!
//! The engine is pure: it holds the policy in force and the progress of one failing prompt,
//! and answers [`Decision`]s. Executing them (sleeping, switching models, sending events) is
//! the backend's job in `run.rs`.

use std::time::Duration;

use nori_protocol::FallbackStep;
use nori_workflow::FailureKind;
use nori_workflow::Recovery;
use nori_workflow::Route;

pub(crate) enum Decision {
    /// Try the same route again after `delay`.
    Retry {
        delay: Duration,
        attempt: i64,
        max_attempts: i64,
    },
    /// Move to `to`; `remaining` are the routes after it, if it cannot be used either.
    Fallback {
        to: Route,
        step: FallbackStep,
        remaining: Vec<Route>,
    },
    /// Nothing in the policy helps: show the failure. `no_route_left` says the policy wanted
    /// a fallback but every route is used up or ruled out.
    Surface { no_route_left: bool },
}

pub(crate) struct Engine {
    recovery: Recovery,
    /// Retries used on the route the prompt currently runs on.
    attempts: i64,
    /// Routes still to try, built when the first fallback is needed.
    plan: Option<std::collections::VecDeque<Route>>,
    /// Routes the plan held when it was built, for "step i of n".
    planned: i64,
    taken: i64,
}

impl Engine {
    pub(crate) fn new(recovery: Recovery) -> Self {
        Self {
            recovery,
            attempts: 0,
            plan: None,
            planned: 0,
            taken: 0,
        }
    }

    pub(crate) fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// Decide what to do about a failure of `kind`. `retry_after` is the provider's own hint;
    /// `unit` is a random number in `[0, 1)` for jitter; `plan` lists the fallback routes when
    /// the first one is needed; `may_fall_back` is false for work that must stay on its route.
    pub(crate) fn decide(
        &mut self,
        kind: FailureKind,
        retry_after: Option<Duration>,
        unit: f64,
        may_fall_back: bool,
        plan: impl FnOnce(&nori_workflow::FallbackPolicy) -> Vec<Route>,
    ) -> Decision {
        let retry = &self.recovery.retry;
        if retry.on.contains(&kind) && self.attempts < retry.max_attempts {
            self.attempts += 1;
            let base = retry.backoff(self.attempts, retry_after);
            let spread = 1.0 + retry.jitter * (2.0 * unit - 1.0);
            return Decision::Retry {
                delay: base.mul_f64(spread.max(0.0)),
                attempt: self.attempts,
                max_attempts: retry.max_attempts,
            };
        }
        if !may_fall_back || !self.recovery.fallback.on.contains(&kind) {
            return Decision::Surface {
                no_route_left: false,
            };
        }
        if self.plan.is_none() {
            let routes = plan(&self.recovery.fallback);
            self.planned = i64::try_from(routes.len()).unwrap_or(i64::MAX);
            self.plan = Some(routes.into());
        }
        self.next_route().unwrap_or(Decision::Surface {
            no_route_left: true,
        })
    }

    /// The next route of the plan, if any remain. Retries used on the route being left do
    /// not carry over to the next one.
    pub(crate) fn next_route(&mut self) -> Option<Decision> {
        let queue = self.plan.as_mut()?;
        let to = queue.pop_front()?;
        self.taken += 1;
        self.attempts = 0;
        Some(Decision::Fallback {
            to,
            step: FallbackStep {
                index: self.taken,
                total: self.planned,
            },
            remaining: queue.iter().cloned().collect(),
        })
    }
}
