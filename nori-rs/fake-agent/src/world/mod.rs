//! The model of a world: serde types for what a script says, a loader for Pkl and JSON, and a
//! validator that points at the step that is wrong.
//!
//! A world is data. The Pkl schema (`schema/World.pkl`, module `nori:World`) and these types are
//! the same shape; Pkl evaluates a script to JSON and serde reads it, so a world written by hand
//! in Pkl and one generated as JSON are the same thing.

mod agent;
mod load;
mod step;
mod tool;
mod validate;
mod value;

pub use agent::AgentDef;
pub use agent::CancelMode;
pub use agent::CancelPolicy;
pub use agent::CapabilityDef;
pub use agent::CommandDef;
pub use agent::Match;
pub use agent::ModeDef;
pub use agent::ModelDef;
pub use agent::Scene;
pub use agent::Startup;
pub use agent::TurnPolicyDef;
pub use agent::Unscripted;
pub use load::WORLD_SCHEMA_PKL;
pub use load::WorldError;
pub use step::*;
pub use tool::*;
pub use value::ChunkUnit;
pub use value::Chunking;
pub use value::Span;
pub use value::Text;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;

/// Virtual time zero of a world that does not say: a fixed instant, so rate-limit reset times
/// are the same on every run.
pub const DEFAULT_START: &str = "2026-11-18T12:00:00Z";

/// One or more fake agents and what they do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct World {
    pub name: String,
    pub description: String,
    /// The instant virtual time starts at, RFC 3339 UTC.
    pub start: String,
    /// How time passes when a person watches the world (`nori --world`): `real`, or a speed such
    /// as `x30`. Absent means `real`. Drivers that test the harness ignore it and use `virtual`.
    pub clock: Option<String>,
    /// The agent a session starts on. Required when the world has several agents, because the
    /// order of a mapping is not a choice anyone made.
    pub default_agent: Option<String>,
    /// Pkl source for the session's `$NORI_HOME/init.pkl`: the recovery policy (retry, fallback
    /// chain) this world is about. Drivers install it in a home they own; a home that already
    /// has an `init.pkl` keeps it.
    pub policy: Option<String>,
    /// Agents by slug. In a Pkl world a mapping keyed by slug; in JSON a mapping or an array of
    /// agents that name their own `slug`.
    #[serde(deserialize_with = "agents_from_map_or_list")]
    pub agents: Vec<AgentDef>,
}

impl Default for World {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            start: DEFAULT_START.to_owned(),
            clock: None,
            default_agent: None,
            policy: None,
            agents: Vec::new(),
        }
    }
}

fn agents_from_map_or_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<AgentDef>, D::Error> {
    match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Object(map) => map
            .into_iter()
            .map(|(slug, agent)| {
                let mut agent: AgentDef =
                    serde_json::from_value(agent).map_err(serde::de::Error::custom)?;
                if agent.slug.is_empty() {
                    agent.slug = slug;
                }
                Ok(agent)
            })
            .collect(),
        other => serde_json::from_value(other).map_err(serde::de::Error::custom),
    }
}

impl World {
    pub fn agent(&self, slug: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|agent| agent.slug == slug)
    }

    /// The agent a session starts on.
    pub fn default_agent(&self) -> Option<&AgentDef> {
        self.default_agent
            .as_deref()
            .and_then(|slug| self.agent(slug))
            .or_else(|| self.agents.first())
    }

    pub fn slugs(&self) -> Vec<&str> {
        self.agents
            .iter()
            .map(|agent| agent.slug.as_str())
            .collect()
    }
}
