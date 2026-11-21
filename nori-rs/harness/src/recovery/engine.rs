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
    /// The route's usage limit is reached and nothing else can take the work: wait until it
    /// resets, then send the same request again on the same route.
    WaitForReset { delay: Duration },
}

/// A network outage is waited out, not reported: once the policy's retries are used, Nori keeps
/// trying at most this far apart until the network is back or the person stops the turn.
const OUTAGE_BACKOFF_CAP: Duration = Duration::from_secs(300);
/// Sent this long after a usage limit says it resets, so the provider agrees it has.
const RESET_SLACK: Duration = Duration::from_secs(30);

pub(crate) struct Engine {
    recovery: Recovery,
    /// Retries used on the route the prompt currently runs on.
    attempts: i64,
    /// Routes still to try, built when the first fallback is needed.
    plan: Option<std::collections::VecDeque<Route>>,
    /// Routes the plan held when it was built, for "step i of n".
    planned: i64,
    taken: i64,
    /// The route the last fallback moved to.
    current: Option<Route>,
}

impl Engine {
    pub(crate) fn new(recovery: Recovery) -> Self {
        Self {
            recovery,
            attempts: 0,
            plan: None,
            planned: 0,
            taken: 0,
            current: None,
        }
    }

    /// An engine for a prompt a fallback handed to this route: the route is already one the
    /// fallback chose.
    pub(crate) fn continuing(recovery: Recovery) -> Self {
        Self {
            taken: 1,
            ..Self::new(recovery)
        }
    }

    pub(crate) fn recovery(&self) -> &Recovery {
        &self.recovery
    }

    /// Decide what to do about a failure of `kind`. `retry_after` is the provider's own hint;
    /// `unit` is a random number in `[0, 1)` for jitter; `plan` lists the fallback routes when
    /// the first one is needed; `may_fall_back` is false for work that must stay on its route.
    ///
    /// Two failures are waited out rather than shown, since nothing is wrong with the work: a
    /// network outage (past the policy's retries, Nori keeps trying, at most five minutes apart,
    /// with `max_attempts` 0 meaning "until it works"), and a usage limit with a known reset
    /// (`reset_in`) when no other route can take the work.
    pub(crate) fn decide(
        &mut self,
        kind: FailureKind,
        retry_after: Option<Duration>,
        reset_in: Option<Duration>,
        unit: f64,
        may_fall_back: bool,
        plan: impl FnOnce(&nori_workflow::FallbackPolicy) -> Vec<Route>,
    ) -> Decision {
        let retry = &self.recovery.retry;
        let spread = 1.0 + retry.jitter * (2.0 * unit - 1.0);
        if retry.on.contains(&kind) && self.attempts < retry.max_attempts {
            self.attempts += 1;
            let base = retry.backoff(self.attempts, retry_after);
            return Decision::Retry {
                delay: base.mul_f64(spread.max(0.0)),
                attempt: self.attempts,
                max_attempts: retry.max_attempts,
            };
        }
        if kind == FailureKind::Network && retry.on.contains(&kind) {
            self.attempts += 1;
            let base = retry
                .backoff(self.attempts, retry_after)
                .max(retry.max_backoff)
                .min(OUTAGE_BACKOFF_CAP);
            return Decision::Retry {
                delay: base.mul_f64(spread.max(0.0)).min(OUTAGE_BACKOFF_CAP),
                attempt: self.attempts,
                max_attempts: 0,
            };
        }
        let wait_for_reset = || {
            (kind == FailureKind::QuotaExhausted)
                .then_some(reset_in)
                .flatten()
                .map(|reset_in| Decision::WaitForReset {
                    delay: reset_in + RESET_SLACK,
                })
        };
        // A route Nori moved to that cannot run at all (its agent is not signed in, missing or
        // broken) says nothing about the model: the person chose the model, not that route. Its
        // agent's other routes share the fault and are passed over too.
        let route_cannot_run = matches!(
            kind,
            FailureKind::Auth | FailureKind::ExecutableNotFound | FailureKind::Initialization
        ) && self.taken > 0;
        if route_cannot_run
            && let (Some(failed), Some(plan)) = (self.current.as_ref(), self.plan.as_mut())
        {
            plan.retain(|route| route.agent != failed.agent);
        }
        if !may_fall_back || !(self.recovery.fallback.on.contains(&kind) || route_cannot_run) {
            return wait_for_reset().unwrap_or(Decision::Surface {
                no_route_left: false,
            });
        }
        if self.plan.is_none() {
            let routes = plan(&self.recovery.fallback);
            self.planned = i64::try_from(routes.len()).unwrap_or(i64::MAX);
            self.plan = Some(routes.into());
        }
        self.next_route()
            .or_else(wait_for_reset)
            .unwrap_or(Decision::Surface {
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
        self.current = Some(to.clone());
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

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn decision(decision: &Decision) -> String {
        match decision {
            Decision::Retry {
                attempt,
                max_attempts,
                ..
            } => format!("retry {attempt}/{max_attempts}"),
            Decision::Fallback { .. } => "fallback".to_owned(),
            Decision::Surface { no_route_left } => format!("surface {no_route_left}"),
            Decision::WaitForReset { delay } => format!("wait {}s", delay.as_secs()),
        }
    }

    #[test]
    fn an_outage_is_waited_out_and_a_spent_limit_waits_for_its_reset() {
        let mut engine = Engine::new(nori_workflow::RecoveryPolicy::default().base);
        let max = engine.recovery().retry.max_attempts;
        let mut seen = Vec::new();
        for _ in 0..max + 3 {
            seen.push(decision(&engine.decide(
                FailureKind::Network,
                None,
                None,
                0.5,
                true,
                |_| Vec::new(),
            )));
        }
        let mut expected: Vec<String> = (1..=max).map(|n| format!("retry {n}/{max}")).collect();
        expected.extend((max + 1..=max + 3).map(|n| format!("retry {n}/0")));
        assert_eq!(seen, expected);

        let mut engine = Engine::new(nori_workflow::RecoveryPolicy::default().base);
        let reset = engine.decide(
            FailureKind::QuotaExhausted,
            None,
            Some(Duration::from_secs(3600)),
            0.5,
            true,
            |_| Vec::new(),
        );
        assert_eq!(decision(&reset), "wait 3630s");
        let mut engine = Engine::new(nori_workflow::RecoveryPolicy::default().base);
        let unknown = engine.decide(FailureKind::QuotaExhausted, None, None, 0.5, true, |_| {
            Vec::new()
        });
        assert_eq!(decision(&unknown), "surface true");
    }

    #[test]
    fn a_fallback_route_whose_agent_cannot_run_gives_way_to_the_next_agent() {
        let route = |agent: &str, model: &str| Route {
            agent: Some(agent.to_owned()),
            model: Some(model.to_owned()),
            ..Route::default()
        };
        let mut engine = Engine::new(nori_workflow::RecoveryPolicy::default().base);
        let plan = vec![
            route("claude-code", "sonnet"),
            route("claude-code", "opus"),
            route("mock-model-alt", "mock-model-fast"),
        ];
        let first = engine.decide(FailureKind::QuotaExhausted, None, None, 0.5, true, |_| plan);
        let Decision::Fallback { to, .. } = first else {
            panic!("expected a fallback");
        };
        assert_eq!(to, route("claude-code", "sonnet"));
        // Claude Code is not signed in: its other model is skipped with it.
        let next = engine.decide(FailureKind::Auth, None, None, 0.5, true, |_| Vec::new());
        let Decision::Fallback { to, .. } = next else {
            panic!("expected a fallback");
        };
        assert_eq!(to, route("mock-model-alt", "mock-model-fast"));
        // Without a fallback in progress, a sign-in failure is the person's to see.
        let mut engine = Engine::new(nori_workflow::RecoveryPolicy::default().base);
        let auth = engine.decide(FailureKind::Auth, None, None, 0.5, true, |_| Vec::new());
        assert_eq!(decision(&auth), "surface false");
    }
}
