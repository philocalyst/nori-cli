//! Read the `Block` shape (`nori:Recovery`) from evaluated Pkl data.
//!
//! One block shape serves policy files, programs, actors and steps: every field is optional
//! and an unset field inherits the layer below. [`RecoveryBlock`] is the serializable form
//! (it travels inside compiled op graphs and journal envelopes); [`RecoveryBlock::to_override`]
//! produces the [`RecoveryOverride`] the engine applies.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::Duration;

use nori_workflow::FailureKind;
use nori_workflow::FallbackOverride;
use nori_workflow::RecoveryLayer;
use nori_workflow::RecoveryOverride;
use nori_workflow::RetryOverride;
use nori_workflow::ReturnToPrimary;
use nori_workflow::Route;
use serde::Deserialize;
use serde::Serialize;

/// A Pkl `Duration` as the evaluator hands it to the host: a number and its unit symbol
/// (`ns`, `us`, `ms`, `s`, `min`, `h`, `d`). A bare number is read as seconds.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DurationValue {
    pub value: f64,
    pub unit: String,
}

impl<'de> Deserialize<'de> for DurationValue {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Seconds(f64),
            Quantity { value: f64, unit: String },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Seconds(value) => Self {
                value,
                unit: "s".to_owned(),
            },
            Raw::Quantity { value, unit } => Self { value, unit },
        })
    }
}

impl DurationValue {
    pub fn to_duration(&self) -> Result<Duration, String> {
        let nanos_per_unit = match self.unit.as_str() {
            "ns" => 1.0,
            "us" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "min" => 60e9,
            "h" => 3600e9,
            "d" => 86_400e9,
            other => return Err(format!("unknown duration unit `{other}`")),
        };
        let nanos = self.value * nanos_per_unit;
        if !nanos.is_finite() || nanos < 0.0 {
            return Err(format!(
                "a duration must be finite and not negative, got {}.{}",
                self.value, self.unit
            ));
        }
        Ok(Duration::from_secs_f64(nanos / 1e9))
    }
}

/// One route: `new Dynamic { agent = "claude-code"; model = "haiku" }`, or the shorthand
/// string `"agent/model"` (a bare `"model"` names a model of the agent already running).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RouteBlock {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub options: BTreeMap<String, String>,
}

impl<'de> Deserialize<'de> for RouteBlock {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Id(String),
            Fields {
                #[serde(default)]
                agent: Option<String>,
                #[serde(default)]
                model: Option<String>,
                #[serde(default)]
                options: BTreeMap<String, String>,
            },
        }
        let route = match Raw::deserialize(deserializer)? {
            Raw::Id(id) => {
                let Route {
                    agent,
                    model,
                    options,
                } = Route::parse_legacy(&id);
                Self {
                    agent,
                    model,
                    options,
                }
            }
            Raw::Fields {
                agent,
                model,
                options,
            } => Self {
                agent,
                model,
                options,
            },
        };
        if route.agent.is_none() && route.model.is_none() {
            return Err(serde::de::Error::custom(
                "a recovery route needs an agent, a model, or both",
            ));
        }
        Ok(route)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetryBlock {
    pub max_attempts: Option<i64>,
    pub initial_backoff: Option<DurationValue>,
    pub max_backoff: Option<DurationValue>,
    pub jitter: Option<f64>,
    pub on: Option<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FallbackBlock {
    pub on: Option<Vec<String>>,
    pub chain: Option<Vec<RouteBlock>>,
    pub ranked: Option<bool>,
    pub prefer_headroom: Option<bool>,
    pub return_to_primary: Option<String>,
}

/// The `Block` class of `nori:Recovery`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecoveryBlock {
    pub retry: Option<RetryBlock>,
    pub fallback: Option<FallbackBlock>,
}

/// The `Layer` class of `nori:Recovery`: a block plus per-role and per-agent overrides.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecoveryLayerBlock {
    pub retry: Option<RetryBlock>,
    pub fallback: Option<FallbackBlock>,
    pub roles: BTreeMap<String, RecoveryBlock>,
    pub agents: BTreeMap<String, RecoveryBlock>,
}

fn kinds(names: &[String], field: &str) -> Result<BTreeSet<FailureKind>, String> {
    names
        .iter()
        .map(|name| {
            FailureKind::parse(name).ok_or_else(|| {
                format!(
                    "`{name}` in {field} is not a failure kind; use one of: {}",
                    FailureKind::ALL.map(FailureKind::name).join(", ")
                )
            })
        })
        .collect()
}

impl RouteBlock {
    pub fn to_route(&self) -> Route {
        Route {
            agent: self.agent.clone(),
            model: self.model.clone(),
            options: self.options.clone(),
        }
    }
}

impl RecoveryBlock {
    /// The override this block states. Fails naming the field when a failure kind, duration
    /// or return-to-primary choice is not valid.
    pub fn to_override(&self) -> Result<RecoveryOverride, String> {
        let mut over = RecoveryOverride::default();
        if let Some(retry) = &self.retry {
            if retry.max_attempts.is_some_and(|attempts| attempts < 0) {
                return Err("retry.max_attempts must not be negative".into());
            }
            if retry
                .jitter
                .is_some_and(|jitter| !(0.0..=1.0).contains(&jitter))
            {
                return Err("retry.jitter must be between 0 and 1".into());
            }
            over.retry = RetryOverride {
                max_attempts: retry.max_attempts,
                initial_backoff: retry
                    .initial_backoff
                    .as_ref()
                    .map(DurationValue::to_duration)
                    .transpose()
                    .map_err(|error| format!("retry.initial_backoff: {error}"))?,
                max_backoff: retry
                    .max_backoff
                    .as_ref()
                    .map(DurationValue::to_duration)
                    .transpose()
                    .map_err(|error| format!("retry.max_backoff: {error}"))?,
                jitter: retry.jitter,
                on: retry
                    .on
                    .as_deref()
                    .map(|names| kinds(names, "retry.on"))
                    .transpose()?,
            };
        }
        if let Some(fallback) = &self.fallback {
            over.fallback = FallbackOverride {
                on: fallback
                    .on
                    .as_deref()
                    .map(|names| kinds(names, "fallback.on"))
                    .transpose()?,
                chain: fallback
                    .chain
                    .as_ref()
                    .map(|chain| chain.iter().map(RouteBlock::to_route).collect()),
                ranked: fallback.ranked,
                prefer_headroom: fallback.prefer_headroom,
                return_to_primary: fallback
                    .return_to_primary
                    .as_deref()
                    .map(|name| {
                        ReturnToPrimary::parse(name).ok_or_else(|| {
                            format!(
                                "`{name}` in fallback.return_to_primary is not valid; use never, \
                                 after_reset or next_session"
                            )
                        })
                    })
                    .transpose()?,
            };
        }
        Ok(over)
    }
}

impl RecoveryLayerBlock {
    pub fn to_layer(&self) -> Result<RecoveryLayer, String> {
        let base = RecoveryBlock {
            retry: self.retry.clone(),
            fallback: self.fallback.clone(),
        }
        .to_override()?;
        let overrides = |blocks: &BTreeMap<String, RecoveryBlock>, field: &str| {
            blocks
                .iter()
                .map(|(name, block)| {
                    block
                        .to_override()
                        .map(|over| (name.clone(), over))
                        .map_err(|error| format!("{field}[\"{name}\"]: {error}"))
                })
                .collect::<Result<BTreeMap<_, _>, String>>()
        };
        Ok(RecoveryLayer {
            base,
            roles: overrides(&self.roles, "roles")?,
            agents: overrides(&self.agents, "agents")?,
        })
    }
}
