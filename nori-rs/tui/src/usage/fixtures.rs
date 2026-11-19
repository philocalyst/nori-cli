//! Fixture usage for the storybook: four providers, a dozen models, quota windows (one pooled)
//! and a price override, all priced by the real price table.

use chrono::DateTime;
use chrono::Utc;
use nori_harness::usage::AccountId;
use nori_harness::usage::AccountLimits;
use nori_harness::usage::AccountModelUsage;
use nori_harness::usage::AccountUsage;
use nori_harness::usage::BucketValue;
use nori_harness::usage::CategoryCost;
use nori_harness::usage::ModelPrice;
use nori_harness::usage::ModelUsage;
use nori_harness::usage::PooledWindow;
use nori_harness::usage::PriceRow;
use nori_harness::usage::PriceSource;
use nori_harness::usage::PriceTable;
use nori_harness::usage::ProviderUsage;
use nori_harness::usage::UsagePeriod;
use nori_harness::usage::UsageSummary;
use nori_protocol::RateLimitKind;
use nori_protocol::RateLimitWindow;
use nori_protocol::UsageTokens;
use nori_tui_components::Theme;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::Metric;
use super::render::Frame;
use super::render::render;
use super::view::ModelKey;
use super::view::Screen;
use super::view::ranked_models;

fn window(
    id: &str,
    label: &str,
    kind: RateLimitKind,
    used: f64,
    minutes: i64,
    resets: &str,
) -> RateLimitWindow {
    RateLimitWindow {
        id: id.to_owned(),
        label: label.to_owned(),
        kind,
        used_percent: used,
        resets_at: Some(resets.to_owned()),
        window_minutes: Some(minutes),
        model_ids: Vec::new(),
    }
}

fn scaled(tokens: &UsageTokens, share: f64) -> UsageTokens {
    let part = |count: i64| (count as f64 * share) as i64;
    UsageTokens {
        input: part(tokens.input),
        output: part(tokens.output),
        cache_read: part(tokens.cache_read),
        cache_write: part(tokens.cache_write),
        reasoning: part(tokens.reasoning),
    }
}

fn pooled(window: RateLimitWindow, sessions: i64, now: DateTime<Utc>) -> PooledWindow {
    PooledWindow {
        window,
        sessions,
        observed_at: now,
        refilled: false,
    }
}

/// `(provider, model, (input, output, cache read, cache write), sessions)`.
type FixtureModel = (&'static str, &'static str, (i64, i64, i64, i64), i64);

const MODELS: [FixtureModel; 11] = [
    (
        "claude",
        "claude-sonnet-4-5",
        (2_100_000, 880_000, 31_400_000, 2_900_000),
        38,
    ),
    (
        "claude",
        "claude-opus-4-5",
        (610_000, 240_000, 6_800_000, 900_000),
        9,
    ),
    (
        "claude",
        "claude-haiku-4-5",
        (900_000, 310_000, 7_200_000, 400_000),
        21,
    ),
    (
        "codex",
        "gpt-5-codex",
        (3_400_000, 760_000, 24_100_000, 0),
        26,
    ),
    (
        "codex",
        "gpt-5-mini",
        (1_200_000, 300_000, 5_000_000, 0),
        14,
    ),
    ("codex", "gpt-5", (420_000, 130_000, 2_300_000, 0), 5),
    (
        "gemini",
        "gemini-2.5-pro",
        (1_800_000, 410_000, 9_700_000, 0),
        11,
    ),
    (
        "gemini",
        "gemini-2.5-flash",
        (2_600_000, 520_000, 11_000_000, 0),
        17,
    ),
    (
        "deepseek",
        "deepseek-chat",
        (3_900_000, 640_000, 18_000_000, 0),
        12,
    ),
    (
        "deepseek",
        "deepseek-reasoner",
        (700_000, 520_000, 2_100_000, 0),
        6,
    ),
    (
        "antigravity",
        "antigravity-pro",
        (480_000, 120_000, 0, 0),
        4,
    ),
];

/// The user's override in the fixture: sonnet billed through a discounted agreement.
const SONNET_OVERRIDE: ModelPrice = ModelPrice {
    input: 2.4,
    output: 12.0,
    cache_read: 0.24,
    cache_write: 3.0,
};

pub(crate) fn fixture_summary(period: UsagePeriod, now: DateTime<Utc>) -> UsageSummary {
    let mut summary = UsageSummary::empty(period, now);
    let table = PriceTable::default();
    let weights = [
        0.55, 0.8, 1.0, 0.7, 0.35, 0.1, 0.9, 0.6, 0.95, 1.0, 0.45, 0.2, 0.85, 0.7,
    ];
    let weight_at = |index: usize, salt: usize| {
        weights[(index * 3 + salt) % weights.len()]
            * (0.6 + 0.8 * (((index * 7 + salt * 5) % 10) as f64 / 10.0))
    };
    let buckets = summary.buckets.len();

    for (salt, (provider, name, (input, output, cache_read, cache_write), sessions)) in
        MODELS.into_iter().enumerate()
    {
        let tokens = UsageTokens {
            input,
            output,
            cache_read,
            cache_write,
            reasoning: output / 5,
        };
        let (price, source) = if name == "claude-sonnet-4-5" {
            (Some(SONNET_OVERRIDE), Some(PriceSource::Override))
        } else {
            match table.price(provider, name) {
                Some((price, source)) => (Some(price), Some(source)),
                None => (None, None),
            }
        };
        let (cost, split) = price.map_or((0.0, (0.0, 0.0, 0.0, 0.0)), |price| {
            let split = price.split_cost(&tokens);
            (split.0 + split.1 + split.2 + split.3, split)
        });
        let sum: f64 = (0..buckets).map(|index| weight_at(index, salt)).sum();
        let series: Vec<BucketValue> = (0..buckets)
            .map(|index| {
                let share = weight_at(index, salt) / sum;
                BucketValue {
                    tokens: (tokens.total() as f64 * share) as i64,
                    cost_usd: cost * share,
                }
            })
            .collect();
        for (bucket, value) in summary.buckets.iter_mut().zip(&series) {
            let slot = bucket.providers.entry(provider.to_owned()).or_default();
            slot.tokens += value.tokens;
            slot.cost_usd += value.cost_usd;
        }
        summary.models.push(ModelUsage {
            provider: provider.to_owned(),
            model: name.to_owned(),
            tokens,
            cost_usd: cost,
            estimated_cost_usd: cost,
            unpriced_tokens: if price.is_none() { tokens.total() } else { 0 },
            sessions,
            category_cost: CategoryCost {
                input: split.0,
                cache_read: split.1,
                cache_write: split.2,
                output: split.3,
            },
            cache_savings_usd: price.map_or(0.0, |price| price.cache_savings(&tokens)),
            series,
            price,
            price_source: source,
        });
    }

    let mut providers: Vec<ProviderUsage> = Vec::new();
    for model in &summary.models {
        match providers
            .iter_mut()
            .find(|usage| usage.provider == model.provider)
        {
            Some(usage) => {
                usage.tokens += model.tokens.total();
                usage.cost_usd += model.cost_usd;
                usage.sessions += model.sessions;
            }
            None => providers.push(ProviderUsage {
                provider: model.provider.clone(),
                tokens: model.tokens.total(),
                cost_usd: model.cost_usd,
                sessions: model.sessions,
            }),
        }
    }
    summary.providers = providers;
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
    summary.totals.sessions = 118;

    let seen: Vec<(String, String)> = summary
        .models
        .iter()
        .map(|model| (model.provider.clone(), model.model.clone()))
        .collect();
    summary.prices = table.rows(&seen);
    for row in &mut summary.prices {
        if row.model == "claude-sonnet-4-5" {
            row.price = SONNET_OVERRIDE;
            row.source = PriceSource::Override;
        }
    }
    summary.prices.push(PriceRow {
        provider: Some("opencode".to_owned()),
        model: "qwen3-coder".to_owned(),
        price: ModelPrice {
            input: 0.2,
            output: 0.8,
            cache_read: 0.2,
            cache_write: 0.2,
        },
        source: PriceSource::Override,
        seen: false,
    });

    let account = |agent: &str, provider: &str| AccountId::derived(agent, provider);
    let limits = |agent: &str, provider: &str, windows: Vec<PooledWindow>| {
        let account = account(agent, provider);
        AccountLimits { account, windows }
    };
    let five = |used, resets| {
        pooled(
            window("five_hour", "5h", RateLimitKind::Session, used, 300, resets),
            1,
            now,
        )
    };
    let week = |used, resets| {
        pooled(
            window(
                "weekly",
                "week",
                RateLimitKind::Weekly,
                used,
                10_080,
                resets,
            ),
            1,
            now,
        )
    };
    summary.limits = vec![
        limits(
            "claude-code",
            "claude",
            vec![
                five(41.0, "2026-11-18T22:14:00Z"),
                week(78.0, "2026-11-21T20:00:00Z"),
            ],
        ),
        limits(
            "claude2",
            "claude",
            vec![
                five(12.0, "2026-11-18T23:40:00Z"),
                week(22.0, "2026-11-23T08:00:00Z"),
            ],
        ),
        limits(
            "codex",
            "codex",
            vec![
                five(12.0, "2026-11-19T00:40:00Z"),
                week(93.0, "2026-11-23T08:00:00Z"),
            ],
        ),
    ];

    // Claude's spend is split across its two profiles and a gateway route; the rest has one
    // account each.
    for provider in &summary.providers {
        let shares: Vec<(AccountId, f64)> = if provider.provider == "claude" {
            vec![
                (account("claude-code", "claude"), 0.62),
                (account("claude2", "claude"), 0.30),
                (account("opencode", "claude"), 0.08),
            ]
        } else {
            let agent = provider.provider.as_str();
            vec![(account(agent, &provider.provider), 1.0)]
        };
        for (id, share) in shares {
            let models: Vec<AccountModelUsage> = summary
                .models
                .iter()
                .filter(|model| model.provider == provider.provider)
                .filter(|_| id.via.is_none() || provider.provider == "claude")
                .take(if id.via.is_some() { 1 } else { usize::MAX })
                .map(|model| AccountModelUsage {
                    model: if id.via.is_some() {
                        format!("anthropic/{}", model.model)
                    } else {
                        model.model.clone()
                    },
                    tokens: scaled(&model.tokens, share),
                    cost_usd: model.cost_usd * share,
                    unpriced_tokens: 0,
                })
                .collect();
            summary.accounts.push(AccountUsage {
                account: id,
                tokens: models.iter().fold(UsageTokens::default(), |sum, model| {
                    sum.saturating_add(&model.tokens)
                }),
                cost_usd: models.iter().map(|model| model.cost_usd).sum(),
                unpriced_tokens: 0,
                sessions: ((provider.sessions as f64) * share).ceil() as i64,
                models,
            });
        }
    }
    summary
}

/// Which screen a specimen shows.
pub enum Specimen {
    /// The overview, with the cursor on the model at this rank (0 is the first), if any.
    Overview {
        selected: Option<usize>,
    },
    /// The detail of the model at this rank.
    Model {
        rank: usize,
    },
    Prices,
}

/// The usage view at `width` x `height` as plain text.
pub fn usage_specimen(
    width: u16,
    height: u16,
    period: UsagePeriod,
    metric: Metric,
    provider: Option<&str>,
    specimen: &Specimen,
) -> String {
    let now = DateTime::parse_from_rfc3339("2026-11-18T20:00:00Z")
        .map_or_else(|_| Utc::now(), |time| time.with_timezone(&Utc));
    let full = fixture_summary(period, now);
    let summary = provider.map_or_else(|| full.clone(), |provider| full.only_provider(provider));
    let key_at = |rank: usize| -> Option<ModelKey> {
        ranked_models(&summary, metric)
            .get(rank)
            .map(|model| (model.provider.clone(), model.model.clone()))
    };
    let (screen, selected) = match specimen {
        Specimen::Overview { selected } => (Screen::Overview, selected.and_then(key_at)),
        Specimen::Model { rank } => {
            let key = key_at(*rank).unwrap_or_default();
            (Screen::Model(key.clone()), Some(key))
        }
        Specimen::Prices => (Screen::Prices, None),
    };
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    render(
        &Frame {
            theme: &Theme::default(),
            period,
            metric,
            provider,
            screen: &screen,
            selected: selected.as_ref(),
            summary: Some(&summary),
            now,
            scroll: 0,
        },
        area,
        &mut buf,
    );
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}
