//! Token usage and rate-limit reporting (the `_meta.nori` contract).
//!
//! Every in-process agent attaches a [`UsageReport`] to the ACP `UsageUpdate` it sends (or to a
//! `session_info_update` when no context size is known), as `_meta.nori`:
//!
//! ```json
//! { "nori": { "model": "claude-haiku-5-5", "account": "you@example.com",
//!             "tokens": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "reasoning": 0 },
//!             "rateLimits": [ { "id": "five_hour", "label": "5h", "usedPercent": 41.0,
//!                               "resetsAt": "2026-11-18T23:00:00Z", "windowMinutes": 300 } ] } }
//! ```
//!
//! The host (`docs/usage.md`) normalizes, stores and displays these; an agent only fills in what
//! its provider told it. A field the provider did not report is absent, never zero. `tokens` are
//! the change since the agent's previous report ([`TokensScope::Delta`]) unless the report says
//! they are the session's running total ([`TokensScope::Session`]); the host differences those.

use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use crate::schema::Meta;

/// The `_meta` key everything here lives under.
const NORI_KEY: &str = "nori";

/// Token counts for one turn or one session, as the provider reported them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenBreakdown {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

impl TokenBreakdown {
    /// Everything the model read and wrote.
    pub fn total(&self) -> i64 {
        [
            self.input,
            self.output,
            self.cache_read,
            self.cache_write,
            self.reasoning,
        ]
        .into_iter()
        .fold(0, i64::saturating_add)
    }

    /// Add `other`'s counts to these.
    pub fn add(&mut self, other: &Self) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.reasoning = self.reasoning.saturating_add(other.reasoning);
    }

    fn to_json(self) -> Value {
        json!({
            "input": self.input,
            "output": self.output,
            "cacheRead": self.cache_read,
            "cacheWrite": self.cache_write,
            "reasoning": self.reasoning,
        })
    }
}

/// What a report's token counts are relative to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TokensScope {
    /// Spent since the agent's previous report.
    #[default]
    Delta,
    /// The session's running total. The host differences successive reports.
    Session,
}

/// One subscription or API usage window, such as Claude's five-hour window.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    /// A stable machine identifier: `five_hour`, `seven_day`, `primary`, `secondary`.
    pub id: String,
    /// A short label for a status line: `5h`, `7d`.
    pub label: String,
    /// How much of the window is used, 0 to 100, when the provider says.
    pub used_percent: Option<f64>,
    /// When the window resets, RFC 3339 in UTC, when the provider says.
    pub resets_at: Option<String>,
    /// How long the window is, in minutes, when the provider says.
    pub window_minutes: Option<i64>,
}

impl RateLimitWindow {
    pub fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            used_percent: None,
            resets_at: None,
            window_minutes: None,
        }
    }

    pub fn used_percent(mut self, percent: Option<f64>) -> Self {
        self.used_percent = percent;
        self
    }

    /// The reset time as seconds since the Unix epoch.
    pub fn resets_at_unix(mut self, seconds: Option<i64>) -> Self {
        self.resets_at = seconds.map(rfc3339_utc);
        self
    }

    pub fn window_minutes(mut self, minutes: Option<i64>) -> Self {
        self.window_minutes = minutes;
        self
    }

    fn to_json(&self) -> Value {
        let mut window = Map::new();
        window.insert("id".to_owned(), json!(self.id));
        window.insert("label".to_owned(), json!(self.label));
        if let Some(percent) = self.used_percent {
            window.insert("usedPercent".to_owned(), json!(percent));
        }
        if let Some(resets_at) = &self.resets_at {
            window.insert("resetsAt".to_owned(), json!(resets_at));
        }
        if let Some(minutes) = self.window_minutes {
            window.insert("windowMinutes".to_owned(), json!(minutes));
        }
        Value::Object(window)
    }
}

/// What a provider reported about usage: the model, the tokens, and the rate-limit windows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageReport {
    pub model: Option<String>,
    /// The account the quota belongs to, when it is known without reading credentials (the
    /// provider's own status says it). Sessions of one account pool their rate-limit windows.
    pub account: Option<String>,
    pub tokens: Option<TokenBreakdown>,
    pub tokens_scope: TokensScope,
    pub rate_limits: Vec<RateLimitWindow>,
}

impl UsageReport {
    /// Whether the provider reported nothing, so nothing is worth sending.
    pub fn is_empty(&self) -> bool {
        self.model.is_none()
            && self.account.is_none()
            && self.tokens.is_none()
            && self.rate_limits.is_empty()
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn account(mut self, account: impl Into<String>) -> Self {
        self.account = Some(account.into());
        self
    }

    /// Token counts spent since the previous report.
    pub fn tokens(mut self, tokens: TokenBreakdown) -> Self {
        self.tokens = Some(tokens);
        self.tokens_scope = TokensScope::Delta;
        self
    }

    /// The session's running token total, for a provider that only reports totals.
    pub fn session_tokens(mut self, tokens: TokenBreakdown) -> Self {
        self.tokens = Some(tokens);
        self.tokens_scope = TokensScope::Session;
        self
    }

    pub fn rate_limit(mut self, window: RateLimitWindow) -> Self {
        self.rate_limits.push(window);
        self
    }

    /// The `_meta` to attach to a `UsageUpdate` or `SessionInfoUpdate`.
    pub fn meta(&self) -> Meta {
        let mut nori = Map::new();
        if let Some(model) = &self.model {
            nori.insert("model".to_owned(), json!(model));
        }
        if let Some(account) = &self.account {
            nori.insert("account".to_owned(), json!(account));
        }
        if let Some(tokens) = self.tokens {
            nori.insert("tokens".to_owned(), tokens.to_json());
            if self.tokens_scope == TokensScope::Session {
                nori.insert("tokensScope".to_owned(), json!("session"));
            }
        }
        if !self.rate_limits.is_empty() {
            nori.insert(
                "rateLimits".to_owned(),
                Value::Array(
                    self.rate_limits
                        .iter()
                        .map(RateLimitWindow::to_json)
                        .collect(),
                ),
            );
        }
        let mut meta = Map::new();
        meta.insert(NORI_KEY.to_owned(), Value::Object(nori));
        meta
    }
}

/// `seconds` since the Unix epoch as an RFC 3339 UTC timestamp, such as `2026-11-18T23:00:00Z`.
pub fn rfc3339_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Howard Hinnant's civil-from-days algorithm.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3_600,
        time % 3_600 / 60,
        time % 60
    )
}
