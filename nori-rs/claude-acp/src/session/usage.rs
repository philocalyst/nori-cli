//! What a session has used, and what Claude says is left (the `_meta.nori` usage contract).
//!
//! Tokens come from the CLI's result message (the whole query's usage) and the context size from
//! the last top-level assistant message. Rate-limit windows come from `rate_limit_event`s: every
//! window seen is kept, the latest event per window wins, and each report carries all of them.

use std::sync::Mutex;
use std::sync::PoisonError;

use nori_acp_kit::ContextUsage;
use nori_acp_kit::RateLimitWindow;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::UsageReport;
use serde_json::Value;

/// The window the CLI's `[1m]` model suffix selects.
const LONG_CONTEXT_TOKENS: i64 = 1_000_000;

#[derive(Debug, Default)]
struct State {
    model: Option<String>,
    tokens: TokenBreakdown,
    context_used: Option<i64>,
    /// The model's context window, as the CLI's result message reports it.
    context_size: Option<i64>,
    windows: Vec<RateLimitWindow>,
}

/// A session's usage so far.
#[derive(Debug, Default)]
pub struct UsageLedger {
    state: Mutex<State>,
}

fn count(usage: &Value, key: &str) -> i64 {
    usage.get(key).and_then(Value::as_i64).unwrap_or(0)
}

impl UsageLedger {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The model the CLI is running, as its assistant messages name it.
    pub fn set_model(&self, model: &str) {
        self.state().model = Some(model.to_owned());
    }

    /// An assistant message's usage is the size of the context of the call it answers.
    pub fn record_context(&self, usage: &Value) {
        let used = [
            "input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
            "output_tokens",
        ]
        .into_iter()
        .map(|key| count(usage, key))
        .fold(0, i64::saturating_add);
        self.state().context_used = Some(used);
    }

    /// A finished query's usage, added to the session's. `model_usage` is the result message's
    /// per-model figures, which carry the model's context window.
    pub fn record_query(&self, usage: &Value, model_usage: Option<&Value>) {
        let mut state = self.state();
        state.tokens.add(&TokenBreakdown {
            input: count(usage, "input_tokens"),
            output: count(usage, "output_tokens"),
            cache_read: count(usage, "cache_read_input_tokens"),
            cache_write: count(usage, "cache_creation_input_tokens"),
            reasoning: usage
                .get("output_tokens_details")
                .map_or(0, |details| count(details, "thinking_tokens")),
        });
        let window = model_usage
            .and_then(Value::as_object)
            .and_then(|models| {
                state
                    .model
                    .as_deref()
                    .and_then(|model| models.get(model))
                    .or_else(|| models.values().next())
            })
            .map(|model| count(model, "contextWindow"))
            .filter(|window| *window > 0);
        if window.is_some() {
            state.context_size = window;
        }
    }

    /// A `rate_limit_event`: remember the windows it describes. Returns whether it described one.
    ///
    /// The CLI sends the window the request counted against (`rateLimitType`, `resetsAt`) and, when
    /// it knows them, every window with its `utilization`, a fraction of the window
    /// (`unifiedWindows: {"five_hour": {"utilization": 0.26, "resetsAt": 1795066800}, ...}`).
    pub fn record_rate_limit(&self, event: &Value) -> bool {
        let Some(info) = event.get("rate_limit_info") else {
            return false;
        };
        let mut windows: Vec<RateLimitWindow> = info
            .get("unifiedWindows")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .map(|(id, window)| {
                rate_limit_window(id, window.get("utilization"), window.get("resetsAt"))
            })
            .collect();
        if let Some(id) = info.get("rateLimitType").and_then(Value::as_str)
            && !windows.iter().any(|window| window.id == id)
        {
            windows.push(rate_limit_window(
                id,
                info.get("utilization"),
                info.get("resetsAt"),
            ));
        }
        let described = !windows.is_empty();
        let mut state = self.state();
        for window in windows {
            match state.windows.iter_mut().find(|known| known.id == window.id) {
                // A window the event names but gives no figures for keeps what was known.
                Some(known) => {
                    *known = RateLimitWindow {
                        used_percent: window.used_percent.or(known.used_percent),
                        resets_at: window.resets_at.clone().or_else(|| known.resets_at.clone()),
                        ..window
                    };
                }
                None => state.windows.push(window),
            }
        }
        described
    }

    /// Everything known, in the shape a usage update carries.
    pub fn report(&self) -> (Option<ContextUsage>, UsageReport) {
        let state = self.state();
        // The CLI's own figure first; before the first result only an explicit long-context model
        // says its window, and anything else is not guessed.
        let size = state.context_size.or_else(|| {
            state
                .model
                .as_deref()
                .and_then(|model| model.contains("[1m]").then_some(LONG_CONTEXT_TOKENS))
        });
        let context = state
            .context_used
            .zip(size)
            .map(|(used, size)| ContextUsage { used, size });
        let mut report = UsageReport::default();
        if let Some(model) = &state.model {
            report = report.model(model);
        }
        if state.tokens.total() > 0 {
            report = report.session_tokens(state.tokens);
        }
        for window in &state.windows {
            report = report.rate_limit(window.clone());
        }
        (context, report)
    }
}

fn rate_limit_window(
    id: &str,
    utilization: Option<&Value>,
    resets_at: Option<&Value>,
) -> RateLimitWindow {
    RateLimitWindow::new(id, window_label(id))
        .used_percent(
            utilization
                .and_then(Value::as_f64)
                .map(|fraction| (fraction * 1000.0).round() / 10.0),
        )
        .window_minutes(window_minutes(id))
        .resets_at_unix(resets_at.and_then(Value::as_i64))
}

fn window_minutes(id: &str) -> Option<i64> {
    match id {
        "five_hour" => Some(5 * 60),
        "seven_day" | "seven_day_opus" | "seven_day_sonnet" => Some(7 * 24 * 60),
        _ => None,
    }
}

fn window_label(id: &str) -> String {
    match id {
        "five_hour" => "5h".to_owned(),
        "seven_day" => "7d".to_owned(),
        "seven_day_opus" => "7d opus".to_owned(),
        "seven_day_sonnet" => "7d sonnet".to_owned(),
        other => other.replace('_', " "),
    }
}
