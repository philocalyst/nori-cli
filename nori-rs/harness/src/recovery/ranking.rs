//! The one ordering of routes, used by every model picker and by ranked fallback.
//!
//! A route sorts by, in order:
//! 1. whether it is the route the session is running now (the user's own choice);
//! 2. its place in the explicit preference list: the recovery chain from policy, then the
//!    `default_models` the user configured, agent by agent;
//! 3. whether its agent is native for the model's family (`claude-code` for Claude, `codex` for
//!    GPT, `gemini`/`antigravity` for Gemini) rather than a gateway to it;
//! 4. the agent's place in the installed-agent order.
//!
//! Between routes to the *same* model on two accounts of one provider (`claude-code` and
//! `claude2`), the room each account has left in its rate-limit windows decides which comes
//! first, when the ranking was given [`AccountHeadroom`]: accounts with more room before accounts
//! with less (in coarse tiers, so near-equal accounts keep the user's order), a spent account
//! last. Only the accounts of one provider trade places, and only among the places they already
//! hold, and the route the session runs on is demoted only when its account is spent: headroom
//! never moves a provider ahead of another, or a gateway ahead of a native agent.
//! Without headroom the order is exactly the one above.
//!
//! A gateway therefore comes first only when the user chose it or named it, or when it is the
//! only agent that offers the model. There is no per-model score table.

use nori_workflow::Route;
use nori_workflow::RouteRef;
use std::collections::HashMap;

use super::headroom::AccountHeadroom;
use super::identity::ModelFamily;
use super::identity::ModelIdentity;
use crate::usage::provider_for;

/// Agents that serve many providers' models behind one login.
const GATEWAYS: [&str; 2] = ["opencode", "openrouter"];

pub fn is_gateway(agent: &str) -> bool {
    GATEWAYS.contains(&agent)
}

/// A route's rank; smaller sorts first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RouteKey {
    not_active: bool,
    preference: usize,
    not_native: bool,
    agent: usize,
}

impl RouteKey {
    /// Whether the user's explicit configuration named this route.
    pub fn is_preferred(&self) -> bool {
        self.preference != usize::MAX
    }
}

#[derive(Debug, Clone, Default)]
pub struct RouteRanking {
    active: Option<RouteRef>,
    preferred: Vec<RouteRef>,
    agent_order: Vec<String>,
    headroom: Option<AccountHeadroom>,
    account_preferences: HashMap<String, Vec<String>>,
}

impl RouteRanking {
    /// `chain` is the recovery policy's explicit routes; `default_models` maps agent to the
    /// model the user configured for it; `installed` lists agents in display order.
    pub fn new(
        active: Option<RouteRef>,
        chain: &[Route],
        default_model: &dyn Fn(&str) -> Option<String>,
        installed: &[String],
    ) -> Self {
        let active_agent = active.as_ref().map(|route| route.agent.as_str());
        let mut agent_order: Vec<String> = active_agent.map(str::to_owned).into_iter().collect();
        for agent in installed {
            if !agent_order.contains(agent) {
                agent_order.push(agent.clone());
            }
        }
        let mut preferred: Vec<RouteRef> = chain
            .iter()
            .filter_map(|route| {
                Some(RouteRef::new(
                    route
                        .agent
                        .clone()
                        .or_else(|| active_agent.map(str::to_owned))?,
                    route.model.clone(),
                ))
            })
            .collect();
        preferred.extend(
            agent_order.iter().filter_map(|agent| {
                Some(RouteRef::new(agent.clone(), Some(default_model(agent)?)))
            }),
        );
        Self {
            active,
            preferred,
            agent_order,
            headroom: None,
            account_preferences: HashMap::new(),
        }
    }

    /// Prefer these account agent slugs within their provider when routes serve one model.
    #[must_use]
    pub fn with_account_preferences(mut self, preferences: HashMap<String, Vec<String>>) -> Self {
        self.account_preferences = preferences;
        self
    }

    /// Let the room each account has left decide between routes to one model.
    #[must_use]
    pub fn with_headroom(mut self, headroom: AccountHeadroom) -> Self {
        self.headroom = Some(headroom);
        self
    }

    /// The headroom this ranking weighs, if it was given any.
    pub fn headroom(&self) -> Option<&AccountHeadroom> {
        self.headroom.as_ref()
    }

    /// The ranking for a session on `config`: its explicit recovery chain (read from the user's
    /// and a trusted project's policy; defaults when they do not load), its `default_models`,
    /// and the installed agents.
    pub fn for_config(config: &nori_config::NoriConfig, active: Option<RouteRef>) -> Self {
        let trusted = config.active_project.trust_level == Some(nori_config::TrustLevel::Trusted);
        let fallback = nori_pkl::load_policy(
            Some(&nori_pkl::global_policy_path(&config.nori_home)),
            trusted
                .then(|| nori_pkl::project_policy_path(&config.cwd))
                .as_deref(),
        )
        .map(|workflow| workflow.recovery.base.fallback)
        .ok();
        let prefer_headroom = fallback
            .as_ref()
            .is_none_or(|fallback| fallback.prefer_headroom);
        let chain = fallback.map(|fallback| fallback.chain).unwrap_or_default();
        let installed: Vec<String> = super::catalog::installed_agents(Some(&config.active_agent))
            .into_iter()
            .map(|info| info.agent_name)
            .collect();
        let ranking = Self::new(
            active,
            &chain,
            &|agent| config.default_models.get(agent).cloned(),
            &installed,
        )
        .with_account_preferences(config.account_preferences.clone());
        if prefer_headroom {
            ranking.with_headroom(AccountHeadroom::load(&config.nori_home))
        } else {
            ranking
        }
    }

    pub fn key(&self, route: &RouteRef, family: Option<ModelFamily>) -> RouteKey {
        let preference = self
            .preferred
            .iter()
            .position(|preferred| names_route(preferred, route))
            .unwrap_or(usize::MAX);
        RouteKey {
            not_active: self.active.as_ref() != Some(route),
            preference,
            not_native: !family.is_some_and(|family| family.is_native(&route.agent)),
            agent: self
                .agent_order
                .iter()
                .position(|agent| agent == &route.agent)
                .unwrap_or(usize::MAX),
        }
    }

    /// Sort `items` best route first, keeping the incoming order among equals.
    pub fn sort<T>(&self, items: &mut [T], route: impl Fn(&T) -> (RouteRef, Option<ModelFamily>)) {
        items.sort_by_cached_key(|item| {
            let (route, family) = route(item);
            self.key(&route, family)
        });
    }

    /// [`Self::sort`] for routes that all serve one model: then, among the routes of each
    /// provider's accounts, the ones with room take the places the group holds first.
    pub fn sort_same_model<T>(
        &self,
        items: &mut [T],
        route: impl Fn(&T) -> (RouteRef, Option<ModelFamily>),
    ) {
        self.sort(items, &route);
        if self.account_preferences.is_empty() && self.headroom.is_none() {
            return;
        }
        let headroom = self.headroom.as_ref();
        // (provider, through a gateway): the accounts that may trade places.
        let peers: Vec<(String, bool)> = items
            .iter()
            .map(|item| {
                let (route, _) = route(item);
                let model = route.model.as_deref().unwrap_or_default();
                (provider_for(&route.agent, model), is_gateway(&route.agent))
            })
            .collect();
        let priority: Vec<(bool, bool, usize, i64)> = items
            .iter()
            .map(|item| {
                let (route, _) = route(item);
                let room = headroom.and_then(|headroom| headroom.room_for(&route));
                let spent = room.as_ref().is_some_and(|room| room.is_spent());
                let provider =
                    provider_for(&route.agent, route.model.as_deref().unwrap_or_default());
                let accounts = self.account_preferences.get(&provider);
                let account_preference = accounts
                    .and_then(|accounts| accounts.iter().position(|agent| agent == &route.agent))
                    .unwrap_or(usize::MAX);
                // A configured account outranks the implicit built-in default. Explicitly
                // selected non-default accounts remain first unless their usage is spent.
                let default_yields = accounts.is_some_and(|accounts| {
                    accounts.iter().any(|agent| agent != &route.agent)
                        && nori_acp_host::accounts::find(&route.agent)
                            .is_some_and(|account| account.is_default())
                });
                let active = self.active.as_ref() == Some(&route) && !spent && !default_yields;
                (
                    spent,
                    !active,
                    account_preference,
                    room.map_or(0, |room| room.tier()),
                )
            })
            .collect();
        let mut wanted: Vec<usize> = (0..items.len()).collect();
        for peer in &peers {
            let places: Vec<usize> = (0..peers.len()).filter(|at| peers[*at] == *peer).collect();
            let mut members = places.clone();
            members.sort_by_key(|member| priority[*member]);
            for (place, member) in places.into_iter().zip(members) {
                wanted[place] = member;
            }
        }
        permute(items, &wanted);
    }
}

/// Put the item that is at `wanted[i]` at `i`, for every `i`.
fn permute<T>(items: &mut [T], wanted: &[usize]) {
    let mut at: Vec<usize> = (0..items.len()).collect();
    let mut position: Vec<usize> = (0..items.len()).collect();
    for (target, want) in wanted.iter().copied().enumerate() {
        let from = position[want];
        if from != target {
            items.swap(target, from);
            let displaced = at[target];
            at[from] = displaced;
            position[displaced] = from;
            at[target] = want;
            position[want] = target;
        }
    }
}

/// Whether a preference names `route`: same agent, and either no model or the same model.
fn names_route(preferred: &RouteRef, route: &RouteRef) -> bool {
    preferred.agent == route.agent
        && match (&preferred.model, &route.model) {
            (None, _) => true,
            (Some(_), None) => false,
            (Some(wanted), Some(found)) => {
                wanted == found
                    || ModelIdentity::of(wanted, wanted)
                        .is_same_model(&ModelIdentity::of(found, found))
            }
        }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use chrono::Utc;
    use nori_protocol::RateLimitKind;
    use nori_protocol::RateLimitWindow;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::usage::AccountId;
    use crate::usage::AccountLimits;
    use crate::usage::PooledWindow;

    fn used(agent: &str, provider: &str, percent: f64) -> AccountLimits {
        let now = Utc::now();
        AccountLimits {
            account: AccountId::derived(agent, provider),
            windows: vec![PooledWindow {
                window: RateLimitWindow {
                    id: "five_hour".to_owned(),
                    label: "5h".to_owned(),
                    kind: RateLimitKind::Session,
                    used_percent: percent,
                    resets_at: Some((now + Duration::hours(2)).to_rfc3339()),
                    window_minutes: Some(300),
                    model_ids: Vec::new(),
                },
                sessions: 1,
                observed_at: now,
                refilled: false,
            }],
        }
    }

    fn order(ranking: &RouteRanking, agents: &[&str], model: &str) -> Vec<String> {
        let mut routes: Vec<RouteRef> = agents
            .iter()
            .map(|agent| RouteRef::new(*agent, Some(model.to_owned())))
            .collect();
        ranking.sort_same_model(&mut routes, |route| (route.clone(), None));
        routes.into_iter().map(|route| route.agent).collect()
    }

    fn ranking(limits: Vec<AccountLimits>) -> RouteRanking {
        let installed: Vec<String> = ["claude-code", "claude2", "codex", "opencode"]
            .map(str::to_owned)
            .to_vec();
        RouteRanking::new(None, &[], &|_| None, &installed)
            .with_headroom(AccountHeadroom::new(limits, Utc::now()))
    }

    fn claude_f_preference(ranking: RouteRanking) -> RouteRanking {
        ranking.with_account_preferences(HashMap::from([(
            "claude".to_owned(),
            vec!["claude@f".to_owned()],
        )]))
    }

    #[test]
    fn accounts_of_one_provider_trade_places_by_room() {
        let ranking = ranking(vec![
            used("claude-code", "claude", 100.0),
            used("claude2", "claude", 12.0),
        ]);
        assert_eq!(
            order(
                &ranking,
                &["claude-code", "claude2", "opencode"],
                "claude-opus-5-5"
            ),
            ["claude2", "claude-code", "opencode"]
        );
    }

    #[test]
    fn little_room_never_lets_a_gateway_pass_the_native_agent() {
        let ranking = ranking(vec![used("codex", "codex", 93.0)]);
        assert_eq!(
            order(&ranking, &["opencode", "codex"], "gpt-6"),
            ["codex", "opencode"]
        );
    }

    #[test]
    fn near_equal_accounts_keep_the_users_order() {
        let ranking = ranking(vec![
            used("claude-code", "claude", 41.0),
            used("claude2", "claude", 12.0),
        ]);
        assert_eq!(
            order(&ranking, &["claude-code", "claude2"], "claude-opus-5-5"),
            ["claude-code", "claude2"]
        );
    }

    #[test]
    fn account_preferences_precede_headroom_only_within_their_provider() {
        let ranking = claude_f_preference(ranking(vec![
            used("claude@f", "claude", 80.0),
            used("claude-code", "claude", 0.0),
            used("openrouter", "claude", 0.0),
        ]));
        assert_eq!(
            order(
                &ranking,
                &["claude-code", "openrouter", "claude@f"],
                "claude-opus-5-5"
            ),
            ["claude@f", "openrouter", "claude-code"]
        );
    }

    #[test]
    fn account_preferences_move_the_implicit_builtin_default_behind_f() {
        let active = RouteRef::new("claude-code", Some("claude-opus-5-5".to_owned()));
        let installed = ["claude-code", "claude@f"].map(str::to_owned).to_vec();
        let ranking =
            claude_f_preference(RouteRanking::new(Some(active), &[], &|_| None, &installed));
        assert_eq!(
            order(&ranking, &["claude@f", "claude-code"], "claude-opus-5-5"),
            ["claude@f", "claude-code"]
        );
    }

    #[test]
    fn account_preferences_preserve_an_active_named_account() {
        let active = RouteRef::new("claude@work", Some("claude-opus-5-5".to_owned()));
        let installed = ["claude@work", "claude@f"].map(str::to_owned).to_vec();
        let ranking =
            claude_f_preference(RouteRanking::new(Some(active), &[], &|_| None, &installed));
        assert_eq!(
            order(&ranking, &["claude@f", "claude@work"], "claude-opus-5-5"),
            ["claude@work", "claude@f"]
        );
    }

    #[test]
    fn known_spent_preferred_account_falls_behind_its_sibling() {
        let ranking = claude_f_preference(ranking(vec![
            used("claude@f", "claude", 99.0),
            used("claude-code", "claude", 90.0),
        ]));
        assert_eq!(
            order(&ranking, &["claude-code", "claude@f"], "claude-opus-5-5"),
            ["claude-code", "claude@f"]
        );
    }
}
