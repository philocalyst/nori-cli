//! Token usage and rate limits from Codex's `TokenCount` events, reported as `_meta.nori`
//! (see `nori_acp_kit::UsageReport`).
//!
//! The tokens are what Codex says the last response cost (a delta, not the thread's running
//! total), as the usage contract wants them. Codex's input count includes the cached part, which
//! the report separates; `reasoning` is already part of `output`, as the contract says. Nothing here is estimated: a
//! field Codex did not send is left out.

use codex_protocol::protocol::RateLimitSnapshot;
use codex_protocol::protocol::RateLimitWindow as CodexRateLimitWindow;
use codex_protocol::protocol::TokenCountEvent;
use codex_protocol::protocol::TokenUsage;
use nori_acp_kit::ContextUsage;
use nori_acp_kit::RateLimitWindow;
use nori_acp_kit::TokenBreakdown;
use nori_acp_kit::UsageReport;

/// Minutes in the windows Codex's plans use, which get the names a status line expects.
const FIVE_HOURS: i64 = 300;
const SEVEN_DAYS: i64 = 10_080;
/// The limit Codex reports for the account's own plan; other limits (a model's) are prefixed.
const DEFAULT_LIMIT_ID: &str = "codex";

/// The context usage and report for one `TokenCount` event. `None` when it carries neither
/// token counts nor rate limits.
pub(super) fn token_count_report(
    model: Option<&str>,
    event: &TokenCountEvent,
) -> Option<(Option<ContextUsage>, UsageReport)> {
    let mut report = UsageReport::default();
    if let Some(model) = model {
        report = report.model(model);
    }
    let mut context = None;
    if let Some(info) = &event.info {
        report = report.tokens(breakdown(&info.last_token_usage));
        context = info.model_context_window.map(|size| ContextUsage {
            used: info.last_token_usage.tokens_in_context_window().max(0),
            size,
        });
    }
    if let Some(snapshot) = &event.rate_limits {
        for window in rate_limit_windows(snapshot) {
            report = report.rate_limit(window);
        }
    }
    (context.is_some() || report.tokens.is_some() || !report.rate_limits.is_empty())
        .then_some((context, report))
}

fn breakdown(usage: &TokenUsage) -> TokenBreakdown {
    TokenBreakdown {
        input: usage.non_cached_input(),
        output: usage.output_tokens.max(0),
        cache_read: usage.cached_input(),
        cache_write: usage.cache_write_input_tokens.max(0),
        reasoning: usage.reasoning_output_tokens.max(0),
    }
}

fn rate_limit_windows(snapshot: &RateLimitSnapshot) -> Vec<RateLimitWindow> {
    let prefix = snapshot
        .limit_id
        .as_deref()
        .filter(|id| *id != DEFAULT_LIMIT_ID)
        .map(|id| format!("{id}."));
    [
        ("primary", &snapshot.primary),
        ("secondary", &snapshot.secondary),
    ]
    .into_iter()
    .filter_map(|(slot, window)| Some((slot, window.as_ref()?)))
    .map(|(slot, window)| {
        let (name, label) = window_names(slot, window);
        let id = format!("{}{name}", prefix.as_deref().unwrap_or_default());
        RateLimitWindow::new(id, label)
            .used_percent(Some(window.used_percent))
            .resets_at_unix(window.resets_at)
            .window_minutes(window.window_minutes)
    })
    .collect()
}

/// A window's id and label: the plans' five-hour and weekly windows by name, any other by its
/// length.
fn window_names(slot: &str, window: &CodexRateLimitWindow) -> (String, String) {
    match window.window_minutes {
        Some(FIVE_HOURS) => ("five_hour".to_owned(), "5h".to_owned()),
        Some(SEVEN_DAYS) => ("seven_day".to_owned(), "7d".to_owned()),
        Some(minutes) if minutes > 0 && minutes % 1_440 == 0 => {
            (slot.to_owned(), format!("{}d", minutes / 1_440))
        }
        Some(minutes) if minutes > 0 && minutes % 60 == 0 => {
            (slot.to_owned(), format!("{}h", minutes / 60))
        }
        Some(minutes) if minutes > 0 => (slot.to_owned(), format!("{minutes}m")),
        Some(_) | None => (slot.to_owned(), slot.to_owned()),
    }
}
