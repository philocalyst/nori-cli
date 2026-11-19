//! Per-million-token prices: a built-in table of list prices and the user's overrides.
//!
//! Overrides live in `$NORI_HOME/config.toml`:
//!
//! ```toml
//! [usage.prices."claude:claude-sonnet-4-5"]
//! input = 3.0
//! output = 15.0
//! cache_read = 0.3      # optional, defaults to `input`
//! cache_write = 3.75    # optional, defaults to `input`
//! ```
//!
//! The key is `"<provider>:<model>"`, or a bare model id to match under every provider. A model
//! with no entry in either place is unpriced: its tokens count, its cost does not.

use std::collections::BTreeMap;
use std::path::Path;

use nori_protocol::UsageTokens;

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl ModelPrice {
    const fn new(input: f64, output: f64, cache_read: f64, cache_write: f64) -> Self {
        Self {
            input,
            output,
            cache_read,
            cache_write,
        }
    }

    /// USD for `tokens`, split by category: `(input, cache_read, cache_write, output)`.
    pub fn split_cost(&self, tokens: &UsageTokens) -> (f64, f64, f64, f64) {
        let per = |count: i64, price: f64| count as f64 * price / 1_000_000.0;
        (
            per(tokens.input, self.input),
            per(tokens.cache_read, self.cache_read),
            per(tokens.cache_write, self.cache_write),
            per(tokens.output, self.output),
        )
    }

    /// What reading cached tokens saved over paying the input price for them.
    pub fn cache_savings(&self, tokens: &UsageTokens) -> f64 {
        tokens.cache_read as f64 * (self.input - self.cache_read).max(0.0) / 1_000_000.0
    }
}

/// Where a model's price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PriceSource {
    Builtin,
    /// The user's `[usage.prices]` entry.
    Override,
}

/// Substrings that must all appear in the model id, most specific entries first.
const BUILTIN: &[(&[&str], ModelPrice)] = &[
    (&["opus"], ModelPrice::new(15.0, 75.0, 1.5, 18.75)),
    (&["sonnet"], ModelPrice::new(3.0, 15.0, 0.3, 3.75)),
    (&["haiku"], ModelPrice::new(1.0, 5.0, 0.1, 1.25)),
    (&["gpt-5", "nano"], ModelPrice::new(0.05, 0.4, 0.005, 0.05)),
    (&["gpt-5", "mini"], ModelPrice::new(0.25, 2.0, 0.025, 0.25)),
    (&["gpt-5"], ModelPrice::new(1.25, 10.0, 0.125, 1.25)),
    (&["codex"], ModelPrice::new(1.25, 10.0, 0.125, 1.25)),
    (&["gpt-4.1"], ModelPrice::new(2.0, 8.0, 0.5, 2.0)),
    (&["gpt-4o"], ModelPrice::new(2.5, 10.0, 1.25, 2.5)),
    (&["o3"], ModelPrice::new(2.0, 8.0, 0.5, 2.0)),
    (&["gemini", "flash"], ModelPrice::new(0.3, 2.5, 0.075, 0.3)),
    (&["gemini"], ModelPrice::new(1.25, 10.0, 0.31, 1.25)),
    (
        &["deepseek", "reason"],
        ModelPrice::new(0.55, 2.19, 0.14, 0.55),
    ),
    (&["deepseek"], ModelPrice::new(0.27, 1.1, 0.07, 0.27)),
    (&["grok"], ModelPrice::new(3.0, 15.0, 0.75, 3.0)),
];

/// One line of the effective price table the usage view shows.
#[derive(Debug, Clone, PartialEq)]
pub struct PriceRow {
    /// `None` for an override keyed by a bare model id, which applies under every provider.
    pub provider: Option<String>,
    pub model: String,
    pub price: ModelPrice,
    pub source: PriceSource,
    /// The model has usage in the period shown.
    pub seen: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    overrides: BTreeMap<String, ModelPrice>,
}

impl PriceTable {
    /// The built-in table plus the overrides in `nori_home`'s config. A missing or unreadable
    /// config only means no overrides.
    pub fn load(nori_home: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(nori_home.join("config.toml")) else {
            return Self::default();
        };
        let Ok(config) = toml::from_str::<toml::Table>(&text) else {
            return Self::default();
        };
        let mut overrides = BTreeMap::new();
        let prices = config
            .get("usage")
            .and_then(toml::Value::as_table)
            .and_then(|usage| usage.get("prices"))
            .and_then(toml::Value::as_table);
        for (key, value) in prices.into_iter().flatten() {
            let Some(table) = value.as_table() else {
                continue;
            };
            let number = |name: &str| {
                table.get(name).and_then(|value| {
                    value
                        .as_float()
                        .or_else(|| value.as_integer().map(|integer| integer as f64))
                })
            };
            let (Some(input), Some(output)) = (number("input"), number("output")) else {
                continue;
            };
            overrides.insert(
                key.to_ascii_lowercase(),
                ModelPrice::new(
                    input,
                    output,
                    number("cache_read").unwrap_or(input),
                    number("cache_write").unwrap_or(input),
                ),
            );
        }
        Self { overrides }
    }

    pub fn price(&self, provider: &str, model: &str) -> Option<(ModelPrice, PriceSource)> {
        let model = model.to_ascii_lowercase();
        let scoped = format!("{provider}:{model}");
        if let Some(price) = self
            .overrides
            .get(&scoped)
            .or_else(|| self.overrides.get(&model))
        {
            return Some((*price, PriceSource::Override));
        }
        BUILTIN
            .iter()
            .find(|(needles, _)| needles.iter().all(|needle| model.contains(needle)))
            .map(|(_, price)| (*price, PriceSource::Builtin))
    }

    /// The effective prices for the models in `seen` (`(provider, model)`), then the overrides
    /// that match none of them, so a configured override is visible before it is used.
    pub fn rows(&self, seen: &[(String, String)]) -> Vec<PriceRow> {
        let mut rows: Vec<PriceRow> = seen
            .iter()
            .filter_map(|(provider, model)| {
                let (price, source) = self.price(provider, model)?;
                Some(PriceRow {
                    provider: Some(provider.clone()),
                    model: model.clone(),
                    price,
                    source,
                    seen: true,
                })
            })
            .collect();
        for (key, price) in &self.overrides {
            let (provider, model) = match key.split_once(':') {
                Some((provider, model)) => (Some(provider.to_owned()), model.to_owned()),
                None => (None, key.clone()),
            };
            let matched = seen.iter().any(|(seen_provider, seen_model)| {
                seen_model.eq_ignore_ascii_case(&model)
                    && provider
                        .as_deref()
                        .is_none_or(|provider| provider == seen_provider)
            });
            if !matched {
                rows.push(PriceRow {
                    provider,
                    model,
                    price: *price,
                    source: PriceSource::Override,
                    seen: false,
                });
            }
        }
        rows
    }
}
