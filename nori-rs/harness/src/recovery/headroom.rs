//! Which account of a provider has the room to take a request.
//!
//! Accounts are what a provider's rate-limit windows belong to (see `usage::account`): two
//! profiles of one agent family, `claude-code` and `claude2`, are two accounts of Claude, each
//! with its own 5h and weekly window. This answers, from the windows the usage log last saw,
//! how much room a route's account has, so ranked fallback and every model picker can prefer
//! the account that can still serve the request and leave a spent one for last.
//!
//! Nothing here is guessed: an account no window was ever reported for has *unknown* room, and
//! unknown ranks like plenty.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use chrono::DateTime;
use chrono::Utc;
use nori_workflow::RouteRef;

use crate::usage::AccountId;
use crate::usage::AccountLimits;
use crate::usage::UsageStore;
use crate::usage::provider_for;
use crate::usage::provider_title;

/// An account with less than this much of its tightest window left is spent: a request sent
/// there is expected to fail, so it is tried after every route that is not.
pub const SPENT_BELOW_PERCENT: f64 = 3.0;

/// How long a read of the usage log stands for the next question.
const FRESH_FOR: Duration = Duration::from_secs(5);

/// What one account has left, by its tightest window.
#[derive(Debug, Clone, PartialEq)]
pub struct Room {
    pub account: AccountId,
    /// Percent of the tightest window still free.
    pub percent_left: f64,
    /// Which window that is (`5h`, `week`).
    pub window: String,
    pub resets_at: Option<DateTime<Utc>>,
}

impl Room {
    fn of(limits: &AccountLimits) -> Option<Self> {
        let tightest = limits.tightest()?;
        Some(Self {
            account: limits.account.clone(),
            percent_left: 100.0 - tightest.used_percent(),
            window: tightest.window.label.clone(),
            resets_at: tightest
                .window
                .resets_at
                .as_deref()
                .and_then(|time| DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.with_timezone(&Utc)),
        })
    }

    pub fn is_spent(&self) -> bool {
        self.percent_left < SPENT_BELOW_PERCENT
    }

    /// Plenty (0), some (1) or little (2) left: coarse on purpose, so two accounts a few points
    /// apart keep the order the user gave them instead of trading places on every report.
    pub fn tier(&self) -> i64 {
        if self.percent_left >= 50.0 {
            0
        } else if self.percent_left >= 20.0 {
            1
        } else {
            2
        }
    }
}

/// The room every known account has, as of one reading of the usage log.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountHeadroom {
    accounts: Vec<AccountLimits>,
    now: Option<DateTime<Utc>>,
}

impl AccountHeadroom {
    pub fn new(accounts: Vec<AccountLimits>, now: DateTime<Utc>) -> Self {
        Self {
            accounts,
            now: Some(now),
        }
    }

    /// Read the usage log under `nori_home`. Blocking, and cached for a few seconds, so a
    /// picker or a plan may call it freely.
    pub fn load(nori_home: &Path) -> Self {
        static CACHE: Mutex<Option<(PathBuf, Instant, AccountHeadroom)>> = Mutex::new(None);
        if let Ok(cache) = CACHE.lock()
            && let Some((home, read_at, headroom)) = cache.as_ref()
            && home == nori_home
            && read_at.elapsed() < FRESH_FOR
        {
            return headroom.clone();
        }
        let now = Utc::now();
        let headroom = Self::new(UsageStore::new(nori_home).limits(now), now);
        if let Ok(mut cache) = CACHE.lock() {
            *cache = Some((nori_home.to_path_buf(), Instant::now(), headroom.clone()));
        }
        headroom
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    /// The account `agent` spends on when it serves `provider`'s models, with its room.
    pub fn room(&self, agent: &str, provider: &str) -> Option<Room> {
        self.accounts
            .iter()
            .find(|limits| {
                limits.provider() == provider
                    && limits.account.agents.iter().any(|known| known == agent)
            })
            .and_then(Room::of)
    }

    /// The room behind a route: the provider is the one its model belongs to.
    pub fn room_for(&self, route: &RouteRef) -> Option<Room> {
        let provider = provider_for(&route.agent, route.model.as_deref().unwrap_or_default());
        self.room(&route.agent, &provider)
    }

    /// The account with the least room left among every known account, for a one-line summary
    /// in a status line: the window to watch.
    pub fn tightest(&self) -> Option<Room> {
        self.accounts
            .iter()
            .filter_map(Room::of)
            .min_by(|a, b| a.percent_left.total_cmp(&b.percent_left))
    }

    /// Whose quota a route spends, as one phrase: `Claude · secondary`, `Claude · via opencode`.
    /// The account the usage log named, or else the one the agent's slug stands for.
    pub fn describe(&self, route: &RouteRef) -> String {
        let provider = provider_for(&route.agent, route.model.as_deref().unwrap_or_default());
        let account = self.room(&route.agent, &provider).map_or_else(
            || AccountId::derived(&route.agent, &provider),
            |room| room.account,
        );
        format!("{} · {}", provider_title(&provider), account.describe())
    }

    /// Of `routes` to one model, the one whose account has the most room. Routes with unknown
    /// room count as plenty; a tie keeps the earlier route.
    pub fn best<'a>(&self, routes: &'a [RouteRef]) -> Option<&'a RouteRef> {
        routes
            .iter()
            .rev()
            .max_by(|a, b| self.percent_left(a).total_cmp(&self.percent_left(b)))
    }

    fn percent_left(&self, route: &RouteRef) -> f64 {
        self.room_for(route).map_or(100.0, |room| room.percent_left)
    }

    /// The moment these readings describe; a reset time before it has passed.
    pub fn as_of(&self) -> Option<DateTime<Utc>> {
        self.now
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration as Span;
    use nori_protocol::RateLimitKind;
    use nori_protocol::RateLimitWindow;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::usage::PooledWindow;

    fn limits(agent: &str, provider: &str, used: f64, now: DateTime<Utc>) -> AccountLimits {
        let mut account = AccountId::derived(agent, provider);
        account.agents = vec![agent.to_owned()];
        AccountLimits {
            account,
            windows: vec![PooledWindow {
                window: RateLimitWindow {
                    id: "five_hour".into(),
                    label: "5h".into(),
                    kind: RateLimitKind::Session,
                    used_percent: used,
                    resets_at: Some((now + Span::hours(2)).to_rfc3339()),
                    window_minutes: Some(300),
                    model_ids: Vec::new(),
                },
                sessions: 1,
                observed_at: now,
                refilled: false,
            }],
        }
    }

    #[test]
    fn the_account_with_room_is_the_one_to_use() {
        let now = Utc::now();
        let headroom = AccountHeadroom::new(
            vec![
                limits("claude-code", "claude", 100.0, now),
                limits("claude2", "claude", 12.0, now),
            ],
            now,
        );
        let routes = [
            RouteRef::new("claude-code", Some("claude-opus-5-5".into())),
            RouteRef::new("claude2", Some("claude-opus-5-5".into())),
        ];
        assert_eq!(headroom.best(&routes), Some(&routes[1]));
        let spent = headroom.room_for(&routes[0]).unwrap();
        assert!(spent.is_spent());
        assert_eq!(spent.account.label, "primary");
        assert_eq!(headroom.room_for(&routes[1]).unwrap().tier(), 0);
    }

    #[test]
    fn an_account_nothing_was_reported_for_has_unknown_room() {
        let now = Utc::now();
        let headroom = AccountHeadroom::new(vec![limits("claude-code", "claude", 80.0, now)], now);
        let unknown = RouteRef::new("claude2", Some("claude-opus-5-5".into()));
        assert_eq!(headroom.room_for(&unknown), None);
        let routes = [
            RouteRef::new("claude-code", Some("claude-opus-5-5".into())),
            unknown,
        ];
        assert_eq!(headroom.best(&routes), Some(&routes[1]));
    }
}
