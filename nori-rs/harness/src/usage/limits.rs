//! Rate-limit windows as the user's accounts see them: one row per window, pooled across every
//! session that reported on the same account.

use std::collections::BTreeMap;
use std::collections::HashMap;

use chrono::DateTime;
use chrono::Utc;
use nori_protocol::RateLimitKind;
use nori_protocol::RateLimitWindow;

use super::account::AccountId;
use super::account::AccountResolver;
use super::account::disambiguate;
use super::sample::UsageSample;

/// One window of one account, merged from every session that reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct PooledWindow {
    /// The most used report among the sessions that still see the window open.
    pub window: RateLimitWindow,
    /// How many sessions reported this window since it last reset.
    pub sessions: i64,
    pub observed_at: DateTime<Utc>,
    /// Its reset time has passed: the window has refilled and the next report will say so.
    pub refilled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AccountLimits {
    pub account: AccountId,
    pub windows: Vec<PooledWindow>,
}

impl AccountLimits {
    pub fn provider(&self) -> &str {
        &self.account.provider
    }

    /// The window with the least left, as used to decide where there is room.
    pub fn tightest(&self) -> Option<&PooledWindow> {
        self.windows
            .iter()
            .max_by(|a, b| a.used_percent().total_cmp(&b.used_percent()))
    }

    /// Percent of the most constrained window still free; `None` when no window is known.
    pub fn headroom(&self) -> Option<f64> {
        self.tightest().map(|window| 100.0 - window.used_percent())
    }
}

impl PooledWindow {
    /// What counts as used now: a refilled window has nothing used.
    pub fn used_percent(&self) -> f64 {
        if self.refilled {
            0.0
        } else {
            self.window.used_percent
        }
    }
}

fn kind_rank(kind: RateLimitKind) -> i64 {
    match kind {
        RateLimitKind::Session => 0,
        RateLimitKind::Weekly => 1,
        RateLimitKind::Monthly => 2,
        RateLimitKind::Other => 3,
    }
}

fn reset_time(window: &RateLimitWindow) -> Option<DateTime<Utc>> {
    window
        .resets_at
        .as_deref()
        .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
        .map(|time| time.with_timezone(&Utc))
}

/// The newest report of each window id by each session, per account.
type NewestBySession<'a> = BTreeMap<
    (String, String),
    (
        AccountId,
        HashMap<(&'a str, &'a str), (&'a RateLimitWindow, DateTime<Utc>)>,
    ),
>;

/// Pool the windows in `samples` as of `now`. Samples that carry no windows are ignored; the
/// newest report of each window by each session stands for that session, so a report that
/// mentions only some of an account's windows leaves the others as they were.
pub(super) fn pool(samples: &[UsageSample], now: DateTime<Utc>) -> Vec<AccountLimits> {
    // (provider, account) -> (session, window) -> newest report.
    let resolver = AccountResolver::new(samples);
    let mut newest: NewestBySession = BTreeMap::new();
    for sample in samples
        .iter()
        .filter(|sample| !sample.rate_limits.is_empty())
    {
        let Some(time) = sample.time() else {
            continue;
        };
        let id = resolver.resolve(sample);
        let (account, sessions) = newest
            .entry((id.provider.clone(), id.key.clone()))
            .or_insert_with(|| (id.clone(), HashMap::new()));
        account.merge_agents(&id.agents);
        for window in &sample.rate_limits {
            let key = (sample.session_id.as_str(), window.id.as_str());
            match sessions.get(&key) {
                Some((_, seen)) if *seen >= time => {}
                _ => {
                    sessions.insert(key, (window, time));
                }
            }
        }
    }

    let mut limits: Vec<AccountLimits> = newest
        .into_values()
        .map(|(account, sessions)| {
            let mut by_window: BTreeMap<&str, Vec<(&RateLimitWindow, DateTime<Utc>)>> =
                BTreeMap::new();
            for ((_, id), (window, time)) in sessions {
                by_window.entry(id).or_default().push((window, time));
            }
            let mut windows: Vec<PooledWindow> = by_window
                .into_values()
                .filter_map(|reports| pool_window(&reports, now))
                .collect();
            windows.sort_by(|a, b| {
                kind_rank(a.window.kind)
                    .cmp(&kind_rank(b.window.kind))
                    .then_with(|| a.window.id.cmp(&b.window.id))
            });
            AccountLimits { account, windows }
        })
        .collect();
    limits.sort_by(|a, b| a.account.cmp(&b.account));
    let mut accounts: Vec<AccountId> = limits.iter().map(|limit| limit.account.clone()).collect();
    disambiguate(&mut accounts);
    for (limit, account) in limits.iter_mut().zip(accounts) {
        limit.account = account;
    }
    limits
}

fn pool_window(
    reports: &[(&RateLimitWindow, DateTime<Utc>)],
    now: DateTime<Utc>,
) -> Option<PooledWindow> {
    let open: Vec<_> = reports
        .iter()
        .filter(|(window, _)| reset_time(window).is_none_or(|reset| reset > now))
        .collect();
    if let Some((best, observed_at)) = open
        .iter()
        .copied()
        .max_by(|(a, _), (b, _)| a.used_percent.total_cmp(&b.used_percent))
    {
        return Some(PooledWindow {
            window: (*best).clone(),
            sessions: i64::try_from(open.len()).unwrap_or(i64::MAX),
            observed_at: *observed_at,
            refilled: false,
        });
    }
    let (newest, observed_at) = reports.iter().max_by_key(|(_, time)| *time)?;
    Some(PooledWindow {
        window: RateLimitWindow {
            used_percent: 0.0,
            ..(*newest).clone()
        },
        sessions: 0,
        observed_at: *observed_at,
        refilled: true,
    })
}
