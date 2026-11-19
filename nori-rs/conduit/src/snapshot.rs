//! The re-loadable Pkl snapshot of a run (`state.pkl`, schema `nori:Run`).

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;
use serde::Serialize;

use crate::error::ConduitError;
use crate::schemas::evaluate_schema_module;
use crate::value::escape_pkl_str;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorState {
    pub agent: String,
    pub model: Option<String>,
    pub session: String,
    pub turns: i64,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorldState {
    pub isolation: String,
    pub root: String,
    pub home: Option<String>,
}

/// A person's answer the run is waiting for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingInterrupt {
    pub id: String,
    pub prompt: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Finished,
    Failed,
    Cancelled,
}

impl RunStatus {
    fn name(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Finished => "finished",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run: String,
    pub program: String,
    pub source: Option<String>,
    pub status: RunStatus,
    pub seq: i64,
    pub tokens: i64,
    pub tool_calls: i64,
    #[serde(default)]
    pub pins: Vec<String>,
    #[serde(default)]
    pub checkpoints: Vec<String>,
    #[serde(default)]
    pub pending_interrupts: Vec<PendingInterrupt>,
    pub actors: BTreeMap<String, ActorState>,
    pub worlds: BTreeMap<String, WorldState>,
    pub bindings: BTreeMap<String, String>,
    pub result: Option<String>,
    pub error: Option<String>,
}

fn quoted(text: &str) -> String {
    format!("\"{}\"", escape_pkl_str(text))
}

fn optional(text: Option<&str>) -> String {
    text.map_or_else(|| "null".to_owned(), quoted)
}

impl RunSnapshot {
    /// The snapshot as a Pkl module that amends `nori:Run`.
    pub fn to_pkl(&self) -> String {
        let mut out = String::from("amends \"nori:Run\"\n\n");
        out.push_str(&format!("run = {}\n", quoted(&self.run)));
        out.push_str(&format!("program = {}\n", quoted(&self.program)));
        out.push_str(&format!("source = {}\n", optional(self.source.as_deref())));
        out.push_str(&format!("status = \"{}\"\n", self.status.name()));
        out.push_str(&format!(
            "seq = {}\ntokens = {}\ntool_calls = {}\n",
            self.seq, self.tokens, self.tool_calls
        ));
        let listing = |items: Vec<String>| format!("new Listing {{ {} }}", items.join("; "));
        out.push_str(&format!(
            "pins = {}\ncheckpoints = {}\n",
            listing(self.pins.iter().map(|item| quoted(item)).collect()),
            listing(self.checkpoints.iter().map(|item| quoted(item)).collect()),
        ));
        out.push_str(&format!(
            "pending_interrupts = {}\n",
            listing(
                self.pending_interrupts
                    .iter()
                    .map(|item| format!(
                        "new {{ id = {}; prompt = {} }}",
                        quoted(&item.id),
                        quoted(&item.prompt)
                    ))
                    .collect()
            )
        ));
        out.push_str("actors {\n");
        for (alias, actor) in &self.actors {
            let fallbacks: Vec<String> = actor.fallbacks.iter().map(|name| quoted(name)).collect();
            out.push_str(&format!(
                "  [{}] {{\n    agent = {}\n    model = {}\n    session = {}\n    turns = {}\n    fallbacks = new Listing {{ {} }}\n  }}\n",
                quoted(alias),
                quoted(&actor.agent),
                optional(actor.model.as_deref()),
                quoted(&actor.session),
                actor.turns,
                fallbacks.join("; ")
            ));
        }
        out.push_str("}\nworlds {\n");
        for (name, world) in &self.worlds {
            out.push_str(&format!(
                "  [{}] {{\n    isolation = {}\n    root = {}\n    home = {}\n  }}\n",
                quoted(name),
                quoted(&world.isolation),
                quoted(&world.root),
                optional(world.home.as_deref())
            ));
        }
        out.push_str("}\nbindings {\n");
        for (name, value) in &self.bindings {
            out.push_str(&format!("  [{}] = {}\n", quoted(name), quoted(value)));
        }
        out.push_str("}\n");
        out.push_str(&format!("result = {}\n", optional(self.result.as_deref())));
        out.push_str(&format!("error = {}\n", optional(self.error.as_deref())));
        out
    }

    /// Replace `path` with this snapshot atomically (write a sibling, then rename).
    pub fn write(&self, path: &Path) -> Result<(), ConduitError> {
        let temporary = path.with_extension("pkl.tmp");
        std::fs::write(&temporary, self.to_pkl())
            .and_then(|()| std::fs::rename(&temporary, path))
            .map_err(|error| ConduitError::Journal {
                message: format!("cannot write snapshot {}: {error}", path.display()),
            })
    }

    /// Reload a snapshot, validating it against the `nori:Run` schema.
    pub fn load(path: &Path) -> Result<Self, ConduitError> {
        let source = std::fs::read_to_string(path).map_err(|error| ConduitError::Journal {
            message: format!("cannot read snapshot {}: {error}", path.display()),
        })?;
        evaluate_schema_module(&source)
    }
}
