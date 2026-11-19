//! Folding samples into what the usage view shows.

use std::collections::BTreeMap;

use chrono::DateTime;
use chrono::Duration;
use chrono::Local;
use chrono::TimeZone;
use chrono::Timelike;
use chrono::Utc;
use nori_protocol::UsageTokens;

use super::account::AccountId;
use super::limits::AccountLimits;
use super::limits::PooledWindow;
use super::pricing::ModelPrice;
use super::pricing::PriceRow;
use super::pricing::PriceSource;

/// How far back the view looks, and how finely it buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsagePeriod {
    /// The last 24 hours, by hour.
    Day,
    /// The last 7 days, by day.
    Week,
    /// The last 30 days, by day.
    Month,
}

impl UsagePeriod {
    pub fn label(self) -> &'static str {
        match self {
            Self::Day => "24h",
            Self::Week => "7d",
            Self::Month => "30d",
        }
    }

    pub fn is_hourly(self) -> bool {
        match self {
            Self::Day => true,
            Self::Week | Self::Month => false,
        }
    }

    pub fn bucket_count(self) -> i64 {
        match self {
            Self::Day => 24,
            Self::Week => 7,
            Self::Month => 30,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CategoryCost {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct UsageTotals {
    pub tokens: UsageTokens,
    pub cost_usd: f64,
    /// The part of `cost_usd` the price table computed, because the provider reported none.
    pub estimated_cost_usd: f64,
    /// Tokens whose model has no price and whose provider reported no cost.
    pub unpriced_tokens: i64,
    /// What reading the cache saved against paying the input price.
    pub cache_savings_usd: f64,
    pub sessions: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct BucketValue {
    pub tokens: i64,
    pub cost_usd: f64,
}

/// One hour or day, per provider.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeBucket {
    pub start: DateTime<Local>,
    pub providers: BTreeMap<String, BucketValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelUsage {
    pub provider: String,
    pub model: String,
    pub tokens: UsageTokens,
    pub cost_usd: f64,
    /// The part of `cost_usd` the price table computed, because the provider reported none.
    pub estimated_cost_usd: f64,
    /// Tokens the price table cannot price and the provider reported no cost for.
    pub unpriced_tokens: i64,
    pub sessions: i64,
    /// What the table prices this model's tokens by, as far as it prices them.
    pub category_cost: CategoryCost,
    pub cache_savings_usd: f64,
    /// Usage per bucket, aligned with [`UsageSummary::buckets`].
    pub series: Vec<BucketValue>,
    /// The price in effect and where it came from; `None` when the model is unpriced.
    pub price: Option<ModelPrice>,
    pub price_source: Option<PriceSource>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderUsage {
    pub provider: String,
    pub tokens: i64,
    pub cost_usd: f64,
    pub sessions: i64,
}

/// What one account spent on one model, as the account's own rows show it.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountModelUsage {
    pub model: String,
    pub tokens: UsageTokens,
    pub cost_usd: f64,
    /// Tokens the price table cannot price and the provider reported no cost for.
    pub unpriced_tokens: i64,
}

/// What one account of one provider spent in the period. Accounts never pool with each other:
/// a gateway's tokens are its own account (`via opencode`) of the provider it routed to.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsage {
    pub account: AccountId,
    pub tokens: UsageTokens,
    pub cost_usd: f64,
    pub unpriced_tokens: i64,
    pub sessions: i64,
    /// By cost, then tokens, descending.
    pub models: Vec<AccountModelUsage>,
}

/// The window that leaves the least room among the accounts in use.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub account: AccountId,
    pub window: PooledWindow,
    /// Another account of the same provider with more room, and how much: the constraint is
    /// softer than it looks when routing can move there.
    pub spare: Option<(AccountId, f64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UsageSummary {
    pub period: UsagePeriod,
    pub since: DateTime<Utc>,
    pub until: DateTime<Utc>,
    pub totals: UsageTotals,
    pub category_cost: CategoryCost,
    pub buckets: Vec<TimeBucket>,
    /// By cost, then tokens, descending.
    pub providers: Vec<ProviderUsage>,
    pub models: Vec<ModelUsage>,
    /// Per provider, then account (the order of [`AccountId`]).
    pub accounts: Vec<AccountUsage>,
    pub limits: Vec<AccountLimits>,
    /// The effective price table: every model seen, then the overrides nothing used yet.
    pub prices: Vec<PriceRow>,
    /// Lines of the usage log that could not be read.
    pub skipped_lines: i64,
}

fn local_midnight(date: chrono::NaiveDate) -> DateTime<Local> {
    Local
        .from_local_datetime(&date.and_time(chrono::NaiveTime::MIN))
        .earliest()
        .unwrap_or_else(|| {
            Utc.from_utc_datetime(&date.and_time(chrono::NaiveTime::MIN))
                .with_timezone(&Local)
        })
}

impl UsageSummary {
    pub fn empty(period: UsagePeriod, now: DateTime<Utc>) -> Self {
        let (since, buckets) = bucket_axis(period, now);
        Self {
            period,
            since,
            until: now,
            totals: UsageTotals::default(),
            category_cost: CategoryCost::default(),
            buckets,
            providers: Vec::new(),
            models: Vec::new(),
            accounts: Vec::new(),
            limits: Vec::new(),
            prices: Vec::new(),
            skipped_lines: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Whether the period has more than one account or any gateway route, so that a per-account
    /// breakdown says something the per-provider one does not.
    pub fn has_account_detail(&self) -> bool {
        let mut seen: Vec<&str> = Vec::new();
        self.accounts.iter().any(|usage| {
            let repeated = seen.contains(&usage.account.provider.as_str());
            seen.push(usage.account.provider.as_str());
            repeated || usage.account.via.is_some()
        })
    }

    /// The tightest window among the accounts that spent anything in the period (every
    /// account with a known window when none did), and the room left on a sibling account.
    pub fn most_constrained(&self) -> Option<Constraint> {
        let in_use = |limits: &&AccountLimits| {
            self.accounts.is_empty()
                || self.accounts.iter().any(|usage| {
                    usage.account.provider == limits.provider()
                        && usage.account.key == limits.account.key
                })
        };
        let (limits, window) = self
            .limits
            .iter()
            .filter(in_use)
            .filter_map(|limits| Some((limits, limits.tightest()?)))
            .max_by(|(_, a), (_, b)| a.used_percent().total_cmp(&b.used_percent()))?;
        let spare = self
            .limits
            .iter()
            .filter(|other| {
                other.provider() == limits.provider() && other.account.key != limits.account.key
            })
            .filter_map(|other| Some((other.account.clone(), other.headroom()?)))
            .filter(|(_, room)| *room > 100.0 - window.used_percent())
            .max_by(|(_, a), (_, b)| a.total_cmp(b));
        Some(Constraint {
            account: limits.account.clone(),
            window: window.clone(),
            spare,
        })
    }

    /// The index of the bucket holding `time`.
    pub(super) fn bucket_index(&self, time: DateTime<Utc>) -> Option<usize> {
        let hourly = self.period.is_hourly();
        let local = time.with_timezone(&Local);
        self.buckets.iter().rposition(|bucket| {
            if hourly {
                bucket.start <= local && local < bucket.start + Duration::hours(1)
            } else {
                bucket.start.date_naive() == local.date_naive()
            }
        })
    }

    /// This summary restricted to one provider: its models, buckets, limits and prices, with
    /// every total recomputed from them.
    pub fn only_provider(&self, provider: &str) -> Self {
        let mut summary = self.clone();
        summary.models.retain(|model| model.provider == provider);
        summary.providers.retain(|usage| usage.provider == provider);
        summary
            .limits
            .retain(|limits| limits.provider() == provider);
        summary
            .accounts
            .retain(|usage| usage.account.provider == provider);
        summary.prices.retain(|row| {
            row.provider
                .as_deref()
                .is_none_or(|row_provider| row_provider == provider)
        });
        for bucket in &mut summary.buckets {
            bucket.providers.retain(|name, _| name == provider);
        }
        let sessions = summary.providers.first().map_or(0, |usage| usage.sessions);
        summary.totals = UsageTotals {
            sessions,
            ..UsageTotals::default()
        };
        summary.category_cost = CategoryCost::default();
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
        summary
    }
}

/// The window start and its empty buckets, oldest first, the last one holding `now`.
pub(super) fn bucket_axis(
    period: UsagePeriod,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, Vec<TimeBucket>) {
    let local_now = now.with_timezone(&Local);
    let empty = |start| TimeBucket {
        start,
        providers: BTreeMap::new(),
    };
    let buckets: Vec<TimeBucket> = if period.is_hourly() {
        let current = local_now
            .with_minute(0)
            .and_then(|time| time.with_second(0))
            .and_then(|time| time.with_nanosecond(0))
            .unwrap_or(local_now);
        (0..period.bucket_count())
            .rev()
            .map(|ago| empty(current - Duration::hours(ago)))
            .collect()
    } else {
        let today = local_now.date_naive();
        (0..period.bucket_count())
            .rev()
            .map(|ago| {
                empty(local_midnight(
                    today - chrono::Days::new(ago.unsigned_abs()),
                ))
            })
            .collect()
    };
    let since = buckets
        .first()
        .map_or(now, |bucket| bucket.start.with_timezone(&Utc));
    (since, buckets)
}
