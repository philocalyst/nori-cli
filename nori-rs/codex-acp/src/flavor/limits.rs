//! The usage windows ChatGPT's Codex backend reports in response headers, as Codex reads them:
//! `x-<limit>-primary-used-percent`, `-window-minutes` and `-reset-at` (Unix seconds; or
//! `-reset-after-seconds`), and the same for `secondary`. `<limit>` is `codex` for the plan's own
//! limits; other families (a model's own limit) get their id as a prefix.

use nori_acp_kit::RateLimitWindow;

/// Minutes in the windows Codex's plans use, which get the names a status line expects.
const FIVE_HOURS: i64 = 300;
const SEVEN_DAYS: i64 = 10_080;
/// The limit Codex reports for the account's own plan.
const DEFAULT_LIMIT: &str = "codex";

/// Every window `headers` report, the plan's own first.
pub(crate) fn windows(headers: &[(String, String)], now: i64) -> Vec<RateLimitWindow> {
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim())
    };
    let mut limits: Vec<String> = headers
        .iter()
        .filter_map(|(name, _)| {
            let name = name.to_ascii_lowercase();
            let limit = name
                .strip_suffix("-primary-used-percent")?
                .strip_prefix("x-")?;
            Some(limit.to_owned())
        })
        .collect();
    limits.sort_by_key(|limit| (limit != DEFAULT_LIMIT, limit.clone()));
    limits.dedup();
    let mut found = Vec::new();
    for limit in limits {
        let prefix = (limit != DEFAULT_LIMIT).then(|| format!("{}.", limit.replace('-', "_")));
        for slot in ["primary", "secondary"] {
            let field = |suffix: &str| header(&format!("x-{limit}-{slot}-{suffix}"));
            let Some(used) = field("used-percent")
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|value| value.is_finite())
            else {
                continue;
            };
            let minutes = field("window-minutes").and_then(|value| value.parse::<i64>().ok());
            let resets_at = field("reset-at")
                .and_then(|value| value.parse::<i64>().ok())
                .or_else(|| {
                    field("reset-after-seconds")
                        .and_then(|value| value.parse::<i64>().ok())
                        .map(|seconds| now.saturating_add(seconds))
                });
            if used == 0.0 && minutes.is_none_or(|minutes| minutes == 0) && resets_at.is_none() {
                continue;
            }
            let (name, label) = names(slot, minutes);
            let id = format!("{}{name}", prefix.as_deref().unwrap_or_default());
            found.push(
                RateLimitWindow::new(id, label)
                    .used_percent(Some(used))
                    .resets_at_unix(resets_at)
                    .window_minutes(minutes),
            );
        }
    }
    found
}

/// A window's id and label: the plans' five-hour and weekly windows by name, any other by its
/// length.
fn names(slot: &str, minutes: Option<i64>) -> (String, String) {
    match minutes {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_plan_windows_and_a_model_limit_come_from_the_headers() {
        let headers: Vec<(String, String)> = [
            ("x-codex-primary-used-percent", "12.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1704069000"),
            ("x-codex-secondary-used-percent", "40"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-after-seconds", "60"),
            ("x-codex-other-primary-used-percent", "0"),
            ("x-gpt-pro-primary-used-percent", "5"),
            ("x-gpt-pro-primary-window-minutes", "60"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        assert_eq!(
            windows(&headers, 1000),
            vec![
                RateLimitWindow::new("five_hour", "5h")
                    .used_percent(Some(12.5))
                    .resets_at_unix(Some(1_704_069_000))
                    .window_minutes(Some(300)),
                RateLimitWindow::new("seven_day", "7d")
                    .used_percent(Some(40.0))
                    .resets_at_unix(Some(1060))
                    .window_minutes(Some(10_080)),
                RateLimitWindow::new("gpt_pro.primary", "1h")
                    .used_percent(Some(5.0))
                    .window_minutes(Some(60)),
            ]
        );
    }
}
