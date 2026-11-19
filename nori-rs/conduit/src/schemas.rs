//! Canonical embedded Pkl schemas (`nori:*`) and typed Rust representations.

use std::collections::BTreeMap;

use nori_pkl::EvaluatorOptions;
use nori_pkl::recovery::DurationValue;
use nori_pkl::recovery::RecoveryBlock;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::ConduitError;

pub const WORKSPACE_SCHEMA_PKL: &str = include_str!("../schemas/Workspace.pkl");
pub const TOOL_SCHEMA_PKL: &str = include_str!("../schemas/Tool.pkl");
pub const SKILL_SCHEMA_PKL: &str = include_str!("../schemas/Skill.pkl");
pub const AGENT_SCHEMA_PKL: &str = include_str!("../schemas/Agent.pkl");
pub const RECOVERY_SCHEMA_PKL: &str = include_str!("../schemas/Recovery.pkl");
pub const RUN_SCHEMA_PKL: &str = include_str!("../schemas/Run.pkl");
pub const PROGRAM_SCHEMA_PKL: &str = include_str!("../schemas/Program.pkl");

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxPolicy {
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub allowed_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    #[serde(default)]
    pub ty: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

/// What a sandboxed tool module may do; see `Capability` in `Tool.pkl`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub capability: String,
    #[serde(default)]
    pub scope: Vec<String>,
}

/// Where a tool's implementation lives.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolSource {
    /// Provided by the host (bash, git, delegation, ...).
    Host,
    /// A WebAssembly module run by the sandboxed tool runtime.
    Wasm {
        file: String,
        #[serde(default = "default_wasm_export")]
        export: String,
        args_schema: String,
        #[serde(default)]
        grants: Vec<Grant>,
        #[serde(default)]
        timeout: Option<DurationValue>,
        #[serde(default)]
        memory_mib: Option<i64>,
    },
}

fn default_wasm_export() -> String {
    "call".to_owned()
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub params: Vec<ToolParam>,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub cost_tokens: i64,
    #[serde(flatten)]
    pub source: ToolSource,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkillSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub instructions: Vec<String>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub required_tools: Vec<ToolSpec>,
}

/// Where an actor runs relative to the session that started it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    #[default]
    Shared,
    Worktree,
    Sandbox,
}

/// An addressable agent session a program starts (`Actor` in `Agent.pkl`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub agent: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub sandbox: Option<SandboxPolicy>,
    #[serde(default)]
    pub recovery: Option<RecoveryBlock>,
    #[serde(default)]
    pub isolation: Isolation,
    #[serde(default)]
    pub world: Option<String>,
}

/// Canonical virtual module map registering all embedded `nori:*` schemas.
pub fn builtin_virtual_modules() -> BTreeMap<String, String> {
    let entries = [
        ("Workspace", WORKSPACE_SCHEMA_PKL),
        ("Tool", TOOL_SCHEMA_PKL),
        ("Skill", SKILL_SCHEMA_PKL),
        ("Agent", AGENT_SCHEMA_PKL),
        ("Recovery", RECOVERY_SCHEMA_PKL),
        ("Program", PROGRAM_SCHEMA_PKL),
        ("Run", RUN_SCHEMA_PKL),
    ];
    let mut modules = BTreeMap::new();
    for (stem, source) in entries {
        modules.insert(format!("nori:{stem}"), source.to_owned());
        modules.insert(format!("nori:{stem}.pkl"), source.to_owned());
        modules.insert(format!("nori:schemas/{stem}.pkl"), source.to_owned());
    }
    modules
}

/// Deterministic `EvaluatorOptions` preloaded with all `nori:*` schemas and
/// restricted resource capabilities (no ambient file or environment reads).
pub fn conduit_evaluator_options() -> EvaluatorOptions {
    EvaluatorOptions::new()
        .allowed_modules(vec!["pkl:".into(), "nori:".into(), "repl:".into()])
        .allowed_resources(vec!["prop:".into()])
        .virtual_modules(builtin_virtual_modules())
}

/// Options for evaluating a program the user wrote: the `nori:*` schemas plus the same module
/// and resource access as policy files (imports next to the program, captured properties).
pub fn program_evaluator_options() -> EvaluatorOptions {
    EvaluatorOptions::preconfigured().virtual_modules(builtin_virtual_modules())
}

/// Evaluate a Pkl source module against the embedded `nori:*` schemas.
pub fn evaluate_schema_module<T: DeserializeOwned>(source: &str) -> Result<T, ConduitError> {
    nori_pkl::native::evaluate(source, &conduit_evaluator_options())
        .map_err(|message| ConduitError::Compilation { message })
}
