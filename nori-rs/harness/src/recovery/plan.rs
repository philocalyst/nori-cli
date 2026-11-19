//! Building the ordered list of routes a failed session may move to.
//!
//! The explicit `chain` from policy comes first, exactly as written. When the policy is
//! `ranked`, every other runnable route follows: first routes that serve the same model as the
//! failing one (native providers before gateways), then other models, all in the order of
//! [`RouteRanking`]. Routes that failed this conversation are never listed.
//!
//! With `prefer_headroom`, routes to the failing model are ordered by the room their account has
//! left (so `claude-code` spent moves to `claude2` before anything cross-provider), and a route
//! whose account is known to be spent is tried after every route that is not.

use nori_workflow::Exclusion;
use nori_workflow::FallbackPolicy;
use nori_workflow::Route;
use nori_workflow::RouteRef;

use super::catalog::Availability;
use super::catalog::CatalogAgent;
use super::identity::ModelIdentity;
use super::ranking::RouteRanking;

pub(crate) struct PlanInput<'a> {
    pub(crate) policy: &'a FallbackPolicy,
    pub(crate) current: &'a RouteRef,
    pub(crate) catalog: &'a [CatalogAgent],
    pub(crate) ranking: &'a RouteRanking,
    pub(crate) excluded: &'a [Exclusion],
}

pub(crate) fn build(input: &PlanInput<'_>) -> Vec<Route> {
    let allowed = |route: &RouteRef| {
        route != input.current && !input.excluded.iter().any(|excluded| excluded.covers(route))
    };
    let mut plan: Vec<Route> = Vec::new();
    let push = |plan: &mut Vec<Route>, route: Route| {
        if !plan.contains(&route) {
            plan.push(route);
        }
    };
    for route in &input.policy.chain {
        let resolved = Route {
            agent: Some(
                route
                    .agent
                    .clone()
                    .unwrap_or_else(|| input.current.agent.clone()),
            ),
            ..route.clone()
        };
        let concrete = RouteRef::new(
            resolved.agent.clone().unwrap_or_default(),
            resolved.model.clone(),
        );
        if allowed(&concrete) {
            push(&mut plan, resolved);
        }
    }
    if !input.policy.ranked {
        return plan;
    }

    let current_identity = input
        .current
        .model
        .as_deref()
        .map(|model| ModelIdentity::of(model, model));
    let mut equivalents: Vec<(RouteRef, Option<super::identity::ModelFamily>)> = Vec::new();
    let mut others: Vec<(RouteRef, Option<super::identity::ModelFamily>)> = Vec::new();
    for agent in input.catalog {
        for model in &agent.models {
            if model.availability != Availability::Runnable {
                continue;
            }
            let route = RouteRef::new(agent.agent.clone(), Some(model.value.clone()));
            if !allowed(&route) {
                continue;
            }
            let entry = (route, model.identity.family());
            if current_identity
                .as_ref()
                .is_some_and(|identity| identity.is_same_model(&model.identity))
            {
                equivalents.push(entry);
            } else {
                others.push(entry);
            }
        }
    }
    // A native provider that has never listed the model is still the right place to try it,
    // under the id a gateway would have prefixed: `openai/gpt-6` is `gpt-6` on codex.
    if let (Some(identity), Some(model)) = (&current_identity, input.current.model.as_deref())
        && let Some(family) = identity.family()
    {
        let bare = model.rsplit_once('/').map_or(model, |(_, bare)| bare);
        for native in input
            .catalog
            .iter()
            .filter(|agent| family.is_native(&agent.agent) && agent.models.is_empty())
        {
            let route = RouteRef::new(native.agent.clone(), Some(bare.to_owned()));
            if allowed(&route) && !equivalents.iter().any(|(known, _)| known == &route) {
                equivalents.push((route, Some(family)));
            }
        }
    }
    if input.policy.prefer_headroom {
        input
            .ranking
            .sort_same_model(&mut equivalents, |(route, family)| (route.clone(), *family));
    } else {
        input
            .ranking
            .sort(&mut equivalents, |(route, family)| (route.clone(), *family));
    }
    input
        .ranking
        .sort(&mut others, |(route, family)| (route.clone(), *family));
    let headroom = input
        .ranking
        .headroom()
        .filter(|_| input.policy.prefer_headroom);
    let (spent, usable): (Vec<_>, Vec<_>) =
        equivalents
            .into_iter()
            .chain(others)
            .partition(|(route, _)| {
                headroom.is_some_and(|headroom| {
                    headroom.room_for(route).is_some_and(|room| room.is_spent())
                })
            });
    for (route, _) in usable.into_iter().chain(spent) {
        push(
            &mut plan,
            Route {
                agent: Some(route.agent),
                model: route.model,
                ..Route::default()
            },
        );
    }
    plan
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use chrono::Utc;
    use nori_protocol::RateLimitKind;
    use nori_protocol::RateLimitWindow;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::recovery::AccountHeadroom;
    use crate::recovery::CatalogModel;
    use crate::usage::AccountId;
    use crate::usage::AccountLimits;
    use crate::usage::PooledWindow;

    fn agent(slug: &str, model: &str) -> CatalogAgent {
        CatalogAgent {
            agent: slug.to_owned(),
            display_name: slug.to_owned(),
            gateway: false,
            models: vec![CatalogModel {
                value: model.to_owned(),
                name: model.to_owned(),
                identity: ModelIdentity::of(model, model),
                availability: Availability::Runnable,
            }],
        }
    }

    fn used(slug: &str, provider: &str, percent: f64) -> AccountLimits {
        let now = Utc::now();
        AccountLimits {
            account: AccountId::derived(slug, provider),
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

    fn plan_with(limits: Vec<AccountLimits>, prefer_headroom: bool) -> Vec<String> {
        let catalog = vec![
            agent("claude-code", "claude-opus-5-5"),
            agent("claude2", "claude-opus-5-5"),
            agent("codex", "gpt-6"),
        ];
        let installed: Vec<String> = catalog.iter().map(|agent| agent.agent.clone()).collect();
        let current = RouteRef::new("claude-code", Some("claude-opus-5-5".to_owned()));
        let policy = FallbackPolicy {
            on: Default::default(),
            chain: Vec::new(),
            ranked: true,
            prefer_headroom,
            return_to_primary: nori_workflow::ReturnToPrimary::NextSession,
        };
        let ranking = RouteRanking::new(Some(current.clone()), &[], &|_| None, &installed)
            .with_headroom(AccountHeadroom::new(limits, Utc::now()));
        build(&PlanInput {
            policy: &policy,
            current: &current,
            catalog: &catalog,
            ranking: &ranking,
            excluded: &[],
        })
        .into_iter()
        .map(|route| route.agent.unwrap_or_default())
        .collect()
    }

    #[test]
    fn a_spent_account_falls_back_to_its_sibling_before_another_provider() {
        let plan = plan_with(
            vec![
                used("claude-code", "claude", 100.0),
                used("claude2", "claude", 20.0),
            ],
            true,
        );
        assert_eq!(plan, ["claude2", "codex"]);
    }

    #[test]
    fn a_sibling_that_is_spent_too_is_tried_last() {
        let plan = plan_with(
            vec![
                used("claude-code", "claude", 100.0),
                used("claude2", "claude", 99.0),
            ],
            true,
        );
        assert_eq!(plan, ["codex", "claude2"]);
    }

    #[test]
    fn without_the_knob_a_spent_sibling_keeps_its_place() {
        let plan = plan_with(
            vec![
                used("claude-code", "claude", 100.0),
                used("claude2", "claude", 99.0),
            ],
            false,
        );
        assert_eq!(plan, ["claude2", "codex"]);
    }
}
