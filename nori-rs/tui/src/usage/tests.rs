//! The accounts and limits sections over a real usage log: two Claude profiles, Codex and a
//! gateway, each with its own windows. What the sections show is the contract of
//! `docs/usage.md#accounts`.

use chrono::DateTime;
use chrono::Duration;
use chrono::Utc;
use nori_harness::usage::UsagePeriod;
use nori_harness::usage::UsageSample;
use nori_harness::usage::UsageStore;
use nori_protocol::RateLimitKind;
use nori_protocol::RateLimitWindow;
use nori_protocol::UsageTokens;
use nori_tui_components::Theme;

use super::Metric;
use super::render::content;
use super::view::Screen;
use super::view::View;

fn window(
    id: &str,
    label: &str,
    kind: RateLimitKind,
    used: f64,
    minutes: i64,
    resets_in: Duration,
    now: DateTime<Utc>,
) -> RateLimitWindow {
    RateLimitWindow {
        id: id.to_owned(),
        label: label.to_owned(),
        kind,
        used_percent: used,
        resets_at: Some((now + resets_in).to_rfc3339()),
        window_minutes: Some(minutes),
        model_ids: Vec::new(),
    }
}

fn sample(
    agent: &str,
    model: &str,
    tokens: i64,
    windows: Vec<RateLimitWindow>,
    now: DateTime<Utc>,
) -> UsageSample {
    UsageSample {
        ts: (now - Duration::minutes(10)).to_rfc3339(),
        session_id: format!("{agent}-1"),
        root_session_id: None,
        actor: None,
        agent: agent.to_owned(),
        provider: nori_harness::usage::provider_for(agent, model),
        model: model.to_owned(),
        tokens: UsageTokens {
            input: tokens,
            output: tokens / 10,
            ..UsageTokens::default()
        },
        cost_usd: Some(tokens as f64 / 100_000.0),
        account: None,
        rate_limits: windows,
    }
}

async fn limits_and_accounts(width: i32) -> Vec<String> {
    let now = DateTime::parse_from_rfc3339("2026-11-18T20:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let home = tempfile::tempdir().unwrap();
    let store = UsageStore::new(home.path());
    let five = |used, resets_in| {
        window(
            "five_hour",
            "5h",
            RateLimitKind::Session,
            used,
            300,
            resets_in,
            now,
        )
    };
    let week = |used, resets_in| {
        window(
            "weekly",
            "week",
            RateLimitKind::Weekly,
            used,
            10_080,
            resets_in,
            now,
        )
    };
    for sample in [
        sample(
            "claude-code",
            "claude-opus-5-5",
            400_000,
            vec![
                five(100.0, Duration::minutes(100)),
                week(78.0, Duration::days(3)),
            ],
            now,
        ),
        sample(
            "claude2",
            "claude-opus-5-5",
            120_000,
            vec![
                five(12.0, Duration::minutes(220)),
                week(22.0, Duration::days(4)),
            ],
            now,
        ),
        sample(
            "opencode",
            "anthropic/claude-opus-5-5",
            50_000,
            Vec::new(),
            now,
        ),
        sample(
            "opencode",
            "opencode-go/deepseek-v4",
            20_000,
            Vec::new(),
            now,
        ),
        sample(
            "codex",
            "gpt-6",
            90_000,
            vec![
                five(12.0, Duration::minutes(280)),
                week(93.0, Duration::days(4)),
            ],
            now,
        ),
    ] {
        store.append(&sample).unwrap();
    }
    let summary = store.summary(UsagePeriod::Week, now).await;
    let theme = Theme::default();
    let view = View {
        theme: &theme,
        selected: None,
        width,
        metric: Metric::Cost,
        now,
        summary: &summary,
    };
    let lines = content(&view, &Screen::Overview).lines;
    let text: Vec<String> = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect();
    let start = text.iter().position(|line| line == "Limits").unwrap();
    let end = text
        .iter()
        .position(|line| line.starts_with("By model") || line == "Models")
        .unwrap();
    text[start..end].to_vec()
}

#[tokio::test]
async fn accounts_sit_side_by_side_at_132_columns() {
    insta::assert_snapshot!(limits_and_accounts(128).await.join("\n"));
}

#[tokio::test]
async fn accounts_fit_80_columns() {
    insta::assert_snapshot!(limits_and_accounts(76).await.join("\n"));
}
