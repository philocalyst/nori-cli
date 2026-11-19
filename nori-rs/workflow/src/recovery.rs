//! Declarative recovery policy: how a session reacts when a request to its model fails.
//!
//! Policy is plain data (written in Pkl, see `docs/workflows.md`); the harness engine in
//! `nori-harness` executes it. A failure is classified into a [`FailureKind`], then:
//!
//! 1. when the kind is in `retry.on` and attempts remain, the same route is tried again after
//!    an exponential, jittered backoff;
//! 2. otherwise, when the kind is in `fallback.on`, the session moves to the next [`Route`];
//! 3. otherwise the failure is surfaced to the user with its cause.
//!
//! Policy layers (built-in default, user, project) are [`RecoveryOverride`]s deep-merged onto
//! one [`RecoveryPolicy`]; `roles` and `agents` carry the same override shape.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

/// What went wrong with a request, without the failure's data. Policy lists these by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    /// The provider throttled the request; waiting helps.
    RateLimited,
    /// The subscription or billing window is used up; only a reset or another route helps.
    QuotaExhausted,
    /// The model does not exist (any more) on this route: HTTP 404, a stale id, a placeholder.
    ModelUnavailable,
    /// The provider has no capacity for this model right now (overloaded, HTTP 529).
    Capacity,
    /// The provider failed internally (HTTP 5xx).
    ServerError,
    /// The agent's backing service could not be reached.
    Network,
    /// Credentials are missing, expired or rejected.
    Auth,
    /// The prompt does not fit the model's context window.
    PromptTooLong,
    /// The agent executable could not be found.
    ExecutableNotFound,
    /// The agent failed to initialise or speak ACP.
    Initialization,
    /// The agent no longer knows the session.
    SessionNotFound,
    /// The session exists but cannot be reattached.
    SessionNotResumable,
    /// The connection has no active session.
    NoActiveSession,
    /// The connection already has an active session.
    SessionAlreadyActive,
    /// Nothing identified the failure.
    Unknown,
}

impl FailureKind {
    pub const ALL: [FailureKind; 15] = [
        FailureKind::RateLimited,
        FailureKind::QuotaExhausted,
        FailureKind::ModelUnavailable,
        FailureKind::Capacity,
        FailureKind::ServerError,
        FailureKind::Network,
        FailureKind::Auth,
        FailureKind::PromptTooLong,
        FailureKind::ExecutableNotFound,
        FailureKind::Initialization,
        FailureKind::SessionNotFound,
        FailureKind::SessionNotResumable,
        FailureKind::NoActiveSession,
        FailureKind::SessionAlreadyActive,
        FailureKind::Unknown,
    ];

    /// The name policy files use, such as `rate_limited`.
    pub fn name(self) -> &'static str {
        match self {
            FailureKind::RateLimited => "rate_limited",
            FailureKind::QuotaExhausted => "quota_exhausted",
            FailureKind::ModelUnavailable => "model_unavailable",
            FailureKind::Capacity => "capacity",
            FailureKind::ServerError => "server_error",
            FailureKind::Network => "network",
            FailureKind::Auth => "auth",
            FailureKind::PromptTooLong => "prompt_too_long",
            FailureKind::ExecutableNotFound => "executable_not_found",
            FailureKind::Initialization => "initialization",
            FailureKind::SessionNotFound => "session_not_found",
            FailureKind::SessionNotResumable => "session_not_resumable",
            FailureKind::NoActiveSession => "no_active_session",
            FailureKind::SessionAlreadyActive => "session_already_active",
            FailureKind::Unknown => "unknown",
        }
    }

    /// A short phrase for notices, such as "rate limited".
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::RateLimited => "rate limited",
            FailureKind::QuotaExhausted => "quota exhausted",
            FailureKind::ModelUnavailable => "model unavailable",
            FailureKind::Capacity => "no capacity",
            FailureKind::ServerError => "server error",
            FailureKind::Network => "network error",
            FailureKind::Auth => "authentication failed",
            FailureKind::PromptTooLong => "prompt too long",
            FailureKind::ExecutableNotFound => "agent not installed",
            FailureKind::Initialization => "agent failed to start",
            FailureKind::SessionNotFound => "session not found",
            FailureKind::SessionNotResumable => "session not resumable",
            FailureKind::NoActiveSession => "no active session",
            FailureKind::SessionAlreadyActive => "session already active",
            FailureKind::Unknown => "request failed",
        }
    }

    /// Parse a policy name. `overloaded` is accepted as the name providers use for `capacity`.
    pub fn parse(name: &str) -> Option<Self> {
        if name == "overloaded" {
            return Some(FailureKind::Capacity);
        }
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// Why a session's route changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteCause {
    /// The route failed in this way and the policy moved the session off it.
    Failure(FailureKind),
    /// The route the user chose can be used again, so the session went back to it.
    PrimaryRestored,
}

impl RouteCause {
    /// A short phrase for the notice row.
    pub fn label(self) -> &'static str {
        match self {
            RouteCause::Failure(kind) => kind.label(),
            RouteCause::PrimaryRestored => "back on your chosen route",
        }
    }
}

/// A place a session can run: an agent and, optionally, one of its models. An unset `agent`
/// means "the agent already running"; an unset `model` means "that agent's own default".
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Route {
    pub agent: Option<String>,
    pub model: Option<String>,
    /// Extra select settings to apply on the route, by setting id, such as reasoning effort.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub options: BTreeMap<String, String>,
}

impl Route {
    /// Parse a legacy `fallback_models` id: `agent/model` names an agent's model (the model id
    /// may contain further `/`, as in `opencode/openai/gpt-6`); a bare id names a model of the
    /// agent already running.
    pub fn parse_legacy(id: &str) -> Self {
        let (agent, model) = match id.split_once('/') {
            Some((agent, model)) => (Some(agent.to_owned()), model.to_owned()),
            None => (None, id.to_owned()),
        };
        Self {
            agent,
            model: Some(model),
            options: BTreeMap::new(),
        }
    }
}

/// Where a session is actually running. Always concrete; never a policy placeholder.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RouteRef {
    pub agent: String,
    pub model: Option<String>,
}

impl RouteRef {
    pub fn new(agent: impl Into<String>, model: Option<String>) -> Self {
        Self {
            agent: agent.into(),
            model,
        }
    }
}

impl std::fmt::Display for RouteRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.model {
            Some(model) => write!(formatter, "{} · {model}", self.agent),
            None => formatter.write_str(&self.agent),
        }
    }
}

/// A route a conversation must not be offered again, learned from a failure.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Exclusion {
    /// Usage is exhausted for `agent` and, on an aggregator that serves several providers, for
    /// the provider its model ids name (`openai` in `openai/gpt-6`).
    Provider {
        agent: String,
        provider: Option<String>,
    },
    /// This exact route failed as unavailable.
    Route(RouteRef),
}

impl Exclusion {
    /// What a quota failure on `route` excludes. `aggregator` agents serve many providers, so
    /// only the failing provider's models go; any other agent is excluded as a whole.
    pub fn exhausted(route: &RouteRef, aggregator: bool) -> Self {
        let provider = aggregator
            .then(|| route.model.as_deref()?.split_once('/'))
            .flatten()
            .map(|(provider, _)| provider.to_owned());
        Exclusion::Provider {
            agent: route.agent.clone(),
            provider,
        }
    }

    pub fn covers(&self, route: &RouteRef) -> bool {
        match self {
            Exclusion::Route(excluded) => excluded == route,
            Exclusion::Provider { agent, provider } => {
                agent == &route.agent
                    && provider.as_deref().is_none_or(|provider| {
                        route
                            .model
                            .as_deref()
                            .and_then(|model| model.split_once('/'))
                            .is_some_and(|(found, _)| found == provider)
                    })
            }
        }
    }
}

/// When the session goes back to the route the user chose after falling back from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnToPrimary {
    /// Stay on the fallback route for the rest of the conversation.
    Never,
    /// Return once the failure that caused the fallback is over (quota windows reset).
    AfterReset,
    /// Start the next session on the primary route again. The conversation in front of the
    /// user keeps running where it is.
    #[default]
    NextSession,
}

impl ReturnToPrimary {
    pub fn name(self) -> &'static str {
        match self {
            ReturnToPrimary::Never => "never",
            ReturnToPrimary::AfterReset => "after_reset",
            ReturnToPrimary::NextSession => "next_session",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        [Self::Never, Self::AfterReset, Self::NextSession]
            .into_iter()
            .find(|choice| choice.name() == name)
    }
}

/// Retrying the same route.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryPolicy {
    /// Retries after the first failure. `0` never retries.
    pub max_attempts: i64,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    /// Fraction of each delay that is randomised, `0.2` meaning plus or minus 20 percent.
    pub jitter: f64,
    pub on: BTreeSet<FailureKind>,
}

impl RetryPolicy {
    /// The wait before retry number `attempt` (1-based): exponential from `initial_backoff`,
    /// capped at `max_backoff`. A provider's own `retry_after` raises it but never past the cap
    /// plus that hint. Jitter is applied by the caller with its own randomness.
    pub fn backoff(&self, attempt: i64, retry_after: Option<Duration>) -> Duration {
        let exponent = u32::try_from(attempt.saturating_sub(1).clamp(0, 30)).unwrap_or(30);
        let exponential = self
            .initial_backoff
            .saturating_mul(2u32.saturating_pow(exponent))
            .min(self.max_backoff);
        retry_after.map_or(exponential, |hint| exponential.max(hint))
    }
}

// `jitter` is validated finite (0 to 1) wherever policy is loaded, so equality is total.
impl Eq for RetryPolicy {}
impl Eq for RetryOverride {}

/// Moving to another route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackPolicy {
    pub on: BTreeSet<FailureKind>,
    /// Routes to move to, in order, before anything ranked.
    pub chain: Vec<Route>,
    /// After the chain, continue with the ranked catalog of every installed agent's models.
    pub ranked: bool,
    /// Among routes to the same model, prefer the account with the most room left in its rate
    /// limit windows, and try an account known to be spent last.
    pub prefer_headroom: bool,
    pub return_to_primary: ReturnToPrimary,
}

/// The recovery behaviour in force for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recovery {
    pub retry: RetryPolicy,
    pub fallback: FallbackPolicy,
}

/// Fields a policy layer sets; unset fields keep the layer below.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryOverride {
    pub retry: RetryOverride,
    pub fallback: FallbackOverride,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct RetryOverride {
    pub max_attempts: Option<i64>,
    pub initial_backoff: Option<Duration>,
    pub max_backoff: Option<Duration>,
    pub jitter: Option<f64>,
    pub on: Option<BTreeSet<FailureKind>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FallbackOverride {
    pub on: Option<BTreeSet<FailureKind>>,
    pub chain: Option<Vec<Route>>,
    pub ranked: Option<bool>,
    pub prefer_headroom: Option<bool>,
    pub return_to_primary: Option<ReturnToPrimary>,
}

impl RecoveryOverride {
    /// Lay `self` over `base`: every field it sets replaces the base's.
    pub fn apply(&self, base: &mut Recovery) {
        let retry = &self.retry;
        if let Some(max_attempts) = retry.max_attempts {
            base.retry.max_attempts = max_attempts;
        }
        if let Some(initial_backoff) = retry.initial_backoff {
            base.retry.initial_backoff = initial_backoff;
        }
        if let Some(max_backoff) = retry.max_backoff {
            base.retry.max_backoff = max_backoff;
        }
        if let Some(jitter) = retry.jitter {
            base.retry.jitter = jitter;
        }
        if let Some(on) = &retry.on {
            base.retry.on.clone_from(on);
        }
        let fallback = &self.fallback;
        if let Some(on) = &fallback.on {
            base.fallback.on.clone_from(on);
        }
        if let Some(chain) = &fallback.chain {
            base.fallback.chain.clone_from(chain);
        }
        if let Some(ranked) = fallback.ranked {
            base.fallback.ranked = ranked;
        }
        if let Some(prefer_headroom) = fallback.prefer_headroom {
            base.fallback.prefer_headroom = prefer_headroom;
        }
        if let Some(return_to_primary) = fallback.return_to_primary {
            base.fallback.return_to_primary = return_to_primary;
        }
    }

    /// Fold `later` into `self`, so applying the result equals applying both in order.
    pub fn merge(&mut self, later: RecoveryOverride) {
        let retry = later.retry;
        self.retry.max_attempts = retry.max_attempts.or(self.retry.max_attempts);
        self.retry.initial_backoff = retry.initial_backoff.or(self.retry.initial_backoff);
        self.retry.max_backoff = retry.max_backoff.or(self.retry.max_backoff);
        self.retry.jitter = retry.jitter.or(self.retry.jitter);
        self.retry.on = retry.on.or(self.retry.on.take());
        let fallback = later.fallback;
        self.fallback.on = fallback.on.or(self.fallback.on.take());
        self.fallback.chain = fallback.chain.or(self.fallback.chain.take());
        self.fallback.ranked = fallback.ranked.or(self.fallback.ranked);
        self.fallback.prefer_headroom = fallback.prefer_headroom.or(self.fallback.prefer_headroom);
        self.fallback.return_to_primary = fallback
            .return_to_primary
            .or(self.fallback.return_to_primary);
    }
}

/// Every layer's recovery policy, merged: the session-wide base plus per-role and per-agent
/// overrides. [`RecoveryPolicy::resolve`] picks what applies to one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryPolicy {
    pub base: Recovery,
    pub roles: BTreeMap<String, RecoveryOverride>,
    pub agents: BTreeMap<String, RecoveryOverride>,
}

impl RecoveryPolicy {
    /// What applies to a session running `agent` in `role` (a subagent's role, if it has one):
    /// the base, then the agent's override, then the role's, so the more specific wins.
    pub fn resolve(&self, agent: &str, role: Option<&str>) -> Recovery {
        let mut recovery = self.base.clone();
        if let Some(over) = self.agents.get(agent) {
            over.apply(&mut recovery);
        }
        if let Some(over) = role.and_then(|role| self.roles.get(role)) {
            over.apply(&mut recovery);
        }
        recovery
    }

    /// Lay one policy layer over the merged policy.
    pub fn apply(&mut self, layer: RecoveryLayer) {
        layer.base.apply(&mut self.base);
        for (role, over) in layer.roles {
            self.roles.entry(role).or_default().merge(over);
        }
        for (agent, over) in layer.agents {
            self.agents.entry(agent).or_default().merge(over);
        }
    }
}

/// What one policy source says about recovery.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryLayer {
    pub base: RecoveryOverride,
    pub roles: BTreeMap<String, RecoveryOverride>,
    pub agents: BTreeMap<String, RecoveryOverride>,
}

impl Default for RecoveryPolicy {
    /// The built-in policy; `pkl/default.pkl` states the same values and a test keeps them equal.
    fn default() -> Self {
        Self {
            base: Recovery {
                retry: RetryPolicy {
                    max_attempts: 4,
                    initial_backoff: Duration::from_secs(2),
                    max_backoff: Duration::from_secs(60),
                    jitter: 0.2,
                    on: [
                        FailureKind::ServerError,
                        FailureKind::Capacity,
                        FailureKind::Network,
                        FailureKind::RateLimited,
                    ]
                    .into(),
                },
                fallback: FallbackPolicy {
                    on: [
                        FailureKind::QuotaExhausted,
                        FailureKind::ModelUnavailable,
                        FailureKind::Capacity,
                    ]
                    .into(),
                    chain: Vec::new(),
                    ranked: true,
                    prefer_headroom: true,
                    return_to_primary: ReturnToPrimary::NextSession,
                },
            },
            roles: BTreeMap::new(),
            agents: BTreeMap::new(),
        }
    }
}
