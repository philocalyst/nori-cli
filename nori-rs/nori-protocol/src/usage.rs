//! What an agent reports about its own consumption: tokens spent and rate-limit windows.
//!
//! In-process agents attach a [`UsageReport`] to the `_meta.nori` object of an ACP `UsageUpdate`
//! (or, when no usage update fits, to a `session_info_update`). The harness normalizes it into
//! samples and stores them; the TUI's usage view renders the stored samples.
//!
//! ```json
//! { "nori": { "model": "claude-haiku-5-5",
//!             "tokens": { "input": 120, "output": 40, "cacheRead": 900, "cacheWrite": 0, "reasoning": 0 },
//!             "rateLimits": [ { "id": "five_hour", "label": "5h", "usedPercent": 41.0,
//!                               "resetsAt": "2026-11-18T23:00:00Z", "windowMinutes": 300 } ] } }
//! ```
//!
//! `account` (optional) names the account the rate limits belong to.
//!
//! `tokens` are the **delta** since the agent's previous report, unless the report carries
//! `"tokensScope": "session"`, in which case they are cumulative for the session and the harness
//! derives the delta (a count that goes down is a reset, and counts as a fresh start).

use serde::Deserialize;
use serde::Serialize;

use crate::acp;

/// Token counts in the five categories every provider can be mapped onto.
///
/// `reasoning` is already part of `output` for every provider that reports it; it is carried
/// separately only so the view can show it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageTokens {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub reasoning: i64,
}

impl UsageTokens {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Every token the provider processed: uncached input, cache traffic and output.
    pub fn total(&self) -> i64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    pub fn saturating_add(&self, other: &Self) -> Self {
        Self {
            input: self.input.saturating_add(other.input),
            output: self.output.saturating_add(other.output),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
            reasoning: self.reasoning.saturating_add(other.reasoning),
        }
    }

    /// `self - previous`, where a category that went down starts over from its new value.
    pub fn delta_since(&self, previous: &Self) -> Self {
        let step = |now: i64, before: i64| if now >= before { now - before } else { now };
        Self {
            input: step(self.input, previous.input),
            output: step(self.output, previous.output),
            cache_read: step(self.cache_read, previous.cache_read),
            cache_write: step(self.cache_write, previous.cache_write),
            reasoning: step(self.reasoning, previous.reasoning),
        }
    }
}

/// Whether reported tokens are new since the last report or the session's running total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokensScope {
    #[default]
    Delta,
    Session,
}

/// How long a rate-limit window lasts, for labeling and pacing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitKind {
    Session,
    Weekly,
    Monthly,
    #[default]
    Other,
}

/// One quota window an account has: how much of it is used and when it resets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RateLimitWindow {
    /// Stable per provider ("five_hour", "weekly", "opus_weekly"); windows merge by it.
    pub id: String,
    /// Short label for display ("5h", "week").
    pub label: String,
    #[serde(default)]
    pub kind: RateLimitKind,
    /// 0 to 100.
    pub used_percent: f64,
    /// RFC 3339 time the window refills.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    /// Length of the window, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<i64>,
    /// When present the window applies only to these models.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_ids: Vec<String>,
}

impl RateLimitWindow {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        let object = value.as_object()?;
        let id = object.get("id")?.as_str()?.to_owned();
        let used_percent = object
            .get("usedPercent")
            .or_else(|| object.get("used_percent"))?
            .as_f64()?
            .clamp(0.0, 100.0);
        let window_minutes = object
            .get("windowMinutes")
            .or_else(|| object.get("windowDurationMins"))
            .and_then(serde_json::Value::as_i64);
        let kind = match object.get("kind").and_then(serde_json::Value::as_str) {
            Some("session") => RateLimitKind::Session,
            Some("weekly") => RateLimitKind::Weekly,
            Some("monthly") => RateLimitKind::Monthly,
            Some(_) => RateLimitKind::Other,
            None => match window_minutes {
                Some(minutes) if minutes <= 24 * 60 => RateLimitKind::Session,
                Some(minutes) if minutes <= 8 * 24 * 60 => RateLimitKind::Weekly,
                Some(_) => RateLimitKind::Monthly,
                None => RateLimitKind::Other,
            },
        };
        Some(Self {
            label: object
                .get("label")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(&id)
                .to_owned(),
            id,
            kind,
            used_percent,
            resets_at: object
                .get("resetsAt")
                .or_else(|| object.get("resets_at"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            window_minutes,
            model_ids: object
                .get("modelIds")
                .and_then(serde_json::Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        })
    }
}

/// One agent report. Every field is optional on the wire; a report with none of them is `None`
/// from [`UsageReport::from_meta`].
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    /// The model that did the work, as the provider names it.
    pub model: Option<String>,
    pub tokens: UsageTokens,
    pub tokens_scope: TokensScope,
    pub rate_limits: Vec<RateLimitWindow>,
    /// Which account the rate limits belong to (an email, a workspace id), so sessions on one
    /// account pool their windows. Absent when the agent cannot tell.
    pub account: Option<String>,
}

impl UsageReport {
    /// Read the report from `_meta.nori`. Missing fields default, unknown fields are ignored, and
    /// a malformed rate-limit window is skipped rather than failing the whole report.
    pub fn from_meta(meta: &acp::v1::Meta) -> Option<Self> {
        let nori = meta.get("nori")?.as_object()?;
        let count = |tokens: &serde_json::Map<String, serde_json::Value>, key: &str| {
            tokens
                .get(key)
                .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|v| v as i64)))
                .unwrap_or(0)
                .max(0)
        };
        let tokens = nori
            .get("tokens")
            .and_then(serde_json::Value::as_object)
            .map(|tokens| UsageTokens {
                input: count(tokens, "input"),
                output: count(tokens, "output"),
                cache_read: count(tokens, "cacheRead"),
                cache_write: count(tokens, "cacheWrite"),
                reasoning: count(tokens, "reasoning"),
            })
            .unwrap_or_default();
        let rate_limits: Vec<RateLimitWindow> = nori
            .get("rateLimits")
            .and_then(serde_json::Value::as_array)
            .map(|windows| {
                windows
                    .iter()
                    .filter_map(RateLimitWindow::from_value)
                    .collect()
            })
            .unwrap_or_default();
        let model = nori
            .get("model")
            .and_then(serde_json::Value::as_str)
            .filter(|model| !model.is_empty())
            .map(str::to_owned);
        if model.is_none() && tokens.is_empty() && rate_limits.is_empty() {
            return None;
        }
        let tokens_scope = match nori.get("tokensScope").and_then(serde_json::Value::as_str) {
            Some("session") => TokensScope::Session,
            Some(_) | None => TokensScope::Delta,
        };
        let account = nori
            .get("account")
            .and_then(serde_json::Value::as_str)
            .filter(|account| !account.is_empty())
            .map(str::to_owned);
        Some(Self {
            model,
            tokens,
            tokens_scope,
            rate_limits,
            account,
        })
    }
}
