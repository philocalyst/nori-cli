//! Folding samples into a [`UsageSummary`].

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use nori_protocol::UsageTokens;

use super::account::AccountId;
use super::account::AccountResolver;
use super::account::disambiguate;
use super::account::without_gateway_echoes;
use super::limits::pool;
use super::pricing::ModelPrice;
use super::pricing::PriceSource;
use super::store::UsageStore;
use super::summary::AccountModelUsage;
use super::summary::AccountUsage;
use super::summary::BucketValue;
use super::summary::CategoryCost;
use super::summary::ModelUsage;
use super::summary::ProviderUsage;
use super::summary::UsagePeriod;
use super::summary::UsageSummary;

/// How long rate-limit reports stay relevant when pooling.
pub(super) const LIMIT_LOOKBACK_DAYS: i64 = 8;

#[derive(Default)]
struct AccountAcc {
    tokens: UsageTokens,
    cost_usd: f64,
    unpriced_tokens: i64,
    sessions: BTreeSet<String>,
    models: BTreeMap<String, (UsageTokens, f64, i64)>,
}

struct ModelAcc {
    tokens: UsageTokens,
    cost_usd: f64,
    estimated_cost_usd: f64,
    unpriced_tokens: i64,
    cache_savings_usd: f64,
    category_cost: CategoryCost,
    sessions: BTreeSet<String>,
    series: Vec<BucketValue>,
    price: Option<(ModelPrice, PriceSource)>,
}

fn count(sessions: usize) -> i64 {
    i64::try_from(sessions).unwrap_or(i64::MAX)
}

impl UsageSummary {
    pub(super) fn build(store: &UsageStore, period: UsagePeriod, now: DateTime<Utc>) -> Self {
        let mut summary = Self::empty(period, now);
        let lookback = now - Duration::days(LIMIT_LOOKBACK_DAYS);
        let (samples, skipped) = store.read_since(summary.since.min(lookback), now);
        let samples = without_gateway_echoes(samples);
        summary.skipped_lines = skipped;
        summary.limits = pool(&samples, now);
        let resolver = AccountResolver::new(&samples);
        let mut by_account: BTreeMap<(String, String), (AccountId, AccountAcc)> = BTreeMap::new();

        let mut models: BTreeMap<(String, String), ModelAcc> = BTreeMap::new();
        let mut provider_sessions: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut all_sessions: BTreeSet<String> = BTreeSet::new();
        let since = summary.since;
        for (sample, time) in samples
            .iter()
            .filter_map(|sample| Some((sample, sample.time()?)))
            .filter(|(_, time)| *time >= since)
        {
            let bucket = summary.bucket_index(time);
            let buckets = summary.buckets.len();
            let acc = models
                .entry((sample.provider.clone(), sample.model.clone()))
                .or_insert_with(|| ModelAcc {
                    tokens: UsageTokens::default(),
                    cost_usd: 0.0,
                    estimated_cost_usd: 0.0,
                    unpriced_tokens: 0,
                    cache_savings_usd: 0.0,
                    category_cost: CategoryCost::default(),
                    sessions: BTreeSet::new(),
                    series: vec![BucketValue::default(); buckets],
                    price: store.prices().price(&sample.provider, &sample.model),
                });
            let tokens = sample.tokens;
            acc.tokens = acc.tokens.saturating_add(&tokens);
            acc.sessions.insert(sample.session_id.clone());
            provider_sessions
                .entry(sample.provider.clone())
                .or_default()
                .insert(sample.session_id.clone());
            all_sessions.insert(sample.session_id.clone());

            let cost = match (sample.cost_usd, acc.price) {
                (Some(reported), _) => Some(reported),
                (None, Some((price, _))) => {
                    let (input, cache_read, cache_write, output) = price.split_cost(&tokens);
                    acc.category_cost.input += input;
                    acc.category_cost.cache_read += cache_read;
                    acc.category_cost.cache_write += cache_write;
                    acc.category_cost.output += output;
                    let estimated = input + cache_read + cache_write + output;
                    acc.estimated_cost_usd += estimated;
                    Some(estimated)
                }
                (None, None) => None,
            };
            if let Some((price, _)) = acc.price {
                acc.cache_savings_usd += price.cache_savings(&tokens);
            }
            match cost {
                Some(cost) => acc.cost_usd += cost,
                None => acc.unpriced_tokens += tokens.total(),
            }
            let id = resolver.resolve(sample);
            let (account, spent) = by_account
                .entry((id.provider.clone(), id.key.clone()))
                .or_insert_with(|| (id.clone(), AccountAcc::default()));
            account.merge_agents(&id.agents);
            spent.tokens = spent.tokens.saturating_add(&tokens);
            spent.cost_usd += cost.unwrap_or(0.0);
            spent.sessions.insert(sample.session_id.clone());
            let unpriced = if cost.is_none() { tokens.total() } else { 0 };
            spent.unpriced_tokens += unpriced;
            let model = spent.models.entry(sample.model.clone()).or_default();
            model.0 = model.0.saturating_add(&tokens);
            model.1 += cost.unwrap_or(0.0);
            model.2 += unpriced;
            if let Some(index) = bucket {
                let value = summary.buckets[index]
                    .providers
                    .entry(sample.provider.clone())
                    .or_default();
                value.tokens += tokens.total();
                value.cost_usd += cost.unwrap_or(0.0);
                acc.series[index].tokens += tokens.total();
                acc.series[index].cost_usd += cost.unwrap_or(0.0);
            }
        }

        let mut accounts: Vec<(AccountId, AccountAcc)> = by_account.into_values().collect();
        accounts.sort_by(|a, b| a.0.cmp(&b.0));
        let mut ids: Vec<AccountId> = accounts.iter().map(|(id, _)| id.clone()).collect();
        disambiguate(&mut ids);
        summary.accounts = accounts
            .into_iter()
            .zip(ids)
            .map(|((_, acc), account)| {
                let mut models: Vec<AccountModelUsage> = acc
                    .models
                    .into_iter()
                    .map(
                        |(model, (tokens, cost_usd, unpriced_tokens))| AccountModelUsage {
                            model,
                            tokens,
                            cost_usd,
                            unpriced_tokens,
                        },
                    )
                    .collect();
                models.sort_by(|a, b| {
                    b.cost_usd
                        .total_cmp(&a.cost_usd)
                        .then_with(|| b.tokens.total().cmp(&a.tokens.total()))
                });
                AccountUsage {
                    account,
                    tokens: acc.tokens,
                    cost_usd: acc.cost_usd,
                    unpriced_tokens: acc.unpriced_tokens,
                    sessions: count(acc.sessions.len()),
                    models,
                }
            })
            .collect();

        let mut by_provider: BTreeMap<String, ProviderUsage> = BTreeMap::new();
        summary.models = models
            .into_iter()
            .map(|((provider, model), acc)| {
                let usage = by_provider
                    .entry(provider.clone())
                    .or_insert_with(|| ProviderUsage {
                        provider: provider.clone(),
                        tokens: 0,
                        cost_usd: 0.0,
                        sessions: provider_sessions
                            .get(&provider)
                            .map_or(0, |sessions| count(sessions.len())),
                    });
                usage.tokens += acc.tokens.total();
                usage.cost_usd += acc.cost_usd;
                ModelUsage {
                    provider,
                    model,
                    tokens: acc.tokens,
                    cost_usd: acc.cost_usd,
                    estimated_cost_usd: acc.estimated_cost_usd,
                    unpriced_tokens: acc.unpriced_tokens,
                    sessions: count(acc.sessions.len()),
                    category_cost: acc.category_cost,
                    cache_savings_usd: acc.cache_savings_usd,
                    series: acc.series,
                    price: acc.price.map(|(price, _)| price),
                    price_source: acc.price.map(|(_, source)| source),
                }
            })
            .collect();
        summary.models.sort_by(|a, b| {
            b.cost_usd
                .total_cmp(&a.cost_usd)
                .then_with(|| b.tokens.total().cmp(&a.tokens.total()))
        });
        summary.providers = by_provider.into_values().collect();
        summary.providers.sort_by(|a, b| {
            b.cost_usd
                .total_cmp(&a.cost_usd)
                .then_with(|| b.tokens.cmp(&a.tokens))
        });

        let seen: Vec<(String, String)> = summary
            .models
            .iter()
            .map(|model| (model.provider.clone(), model.model.clone()))
            .collect();
        summary.prices = store.prices().rows(&seen);

        // Totals are the models' sums, so a provider-filtered summary agrees with them.
        for model in &summary.models {
            summary.totals.tokens = summary.totals.tokens.saturating_add(&model.tokens);
            summary.totals.cost_usd += model.cost_usd;
            summary.totals.estimated_cost_usd += model.estimated_cost_usd;
            summary.totals.unpriced_tokens += model.unpriced_tokens;
            summary.totals.cache_savings_usd += model.cache_savings_usd;
            summary.category_cost.input += model.category_cost.input;
            summary.category_cost.cache_read += model.category_cost.cache_read;
            summary.category_cost.cache_write += model.category_cost.cache_write;
            summary.category_cost.output += model.category_cost.output;
        }
        summary.totals.sessions = count(all_sessions.len());
        summary
    }
}

#[cfg(test)]
mod tests {
    use nori_protocol::RateLimitKind;
    use nori_protocol::RateLimitWindow;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::usage::UsageSample;

    fn window(used: f64, now: DateTime<Utc>) -> RateLimitWindow {
        RateLimitWindow {
            id: "five_hour".to_owned(),
            label: "5h".to_owned(),
            kind: RateLimitKind::Session,
            used_percent: used,
            resets_at: Some((now + Duration::hours(2)).to_rfc3339()),
            window_minutes: Some(300),
            model_ids: Vec::new(),
        }
    }

    fn sample(
        agent: &str,
        model: &str,
        session: &str,
        tokens: i64,
        used: Option<f64>,
        now: DateTime<Utc>,
    ) -> UsageSample {
        UsageSample {
            ts: (now - Duration::minutes(5)).to_rfc3339(),
            session_id: session.to_owned(),
            root_session_id: None,
            actor: None,
            agent: agent.to_owned(),
            provider: crate::usage::provider_for(agent, model),
            model: model.to_owned(),
            tokens: UsageTokens {
                input: tokens,
                ..UsageTokens::default()
            },
            cost_usd: Some(tokens as f64 / 1000.0),
            account: None,
            rate_limits: used.map(|used| window(used, now)).into_iter().collect(),
        }
    }

    #[test]
    fn accounts_of_one_provider_never_pool_and_a_gateway_is_its_own_account() {
        let now = Utc::now();
        let home = tempfile::tempdir().unwrap();
        let store = UsageStore::new(home.path());
        for sample in [
            sample("claude-code", "claude-opus-5-5", "a", 2000, Some(41.0), now),
            sample("claude2", "claude-opus-5-5", "b", 500, Some(12.0), now),
            sample("opencode", "anthropic/claude-opus-5-5", "c", 300, None, now),
            sample("opencode", "opencode-go/deepseek-v4", "d", 100, None, now),
        ] {
            store.append(&sample).unwrap();
        }
        let summary = UsageSummary::build(&store, UsagePeriod::Day, now);

        let accounts: Vec<(String, String, Option<String>, i64)> = summary
            .accounts
            .iter()
            .map(|usage| {
                (
                    usage.account.provider.clone(),
                    usage.account.label.clone(),
                    usage.account.via.clone(),
                    usage.tokens.total(),
                )
            })
            .collect();
        assert_eq!(
            accounts,
            [
                ("claude".to_owned(), "primary".to_owned(), None, 2000),
                ("claude".to_owned(), "secondary".to_owned(), None, 500),
                (
                    "claude".to_owned(),
                    "via opencode".to_owned(),
                    Some("opencode".to_owned()),
                    300
                ),
                (
                    "opencode-go".to_owned(),
                    "via opencode".to_owned(),
                    Some("opencode".to_owned()),
                    100
                ),
            ]
        );

        let windows: Vec<(String, f64)> = summary
            .limits
            .iter()
            .map(|limits| {
                (
                    limits.account.label.clone(),
                    limits.windows[0].window.used_percent,
                )
            })
            .collect();
        assert_eq!(
            windows,
            [("primary".to_owned(), 41.0), ("secondary".to_owned(), 12.0)]
        );

        let tight = summary.most_constrained().unwrap();
        assert_eq!(tight.account.label, "primary");
        assert_eq!(
            tight.spare.map(|(account, room)| (account.label, room)),
            Some(("secondary".to_owned(), 88.0))
        );
        assert!(summary.has_account_detail());
    }
}
