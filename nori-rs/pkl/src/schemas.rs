//! The schemas of the declarative kinds, embedded as virtual modules.
//!
//! A module says what it is with `amends "nori:<Kind>"`. [`EvaluatorOptions::with_runtime_schemas`]
//! makes `nori:Job`, `nori:Monitor`, `nori:Plugin` and `nori:Gate` importable. `nori:Skill` belongs to
//! `nori-conduit`, which supplies it through the caller's [`EvaluatorOptions`].

use std::collections::BTreeMap;

use crate::EvaluatorOptions;

/// The Pkl schema of a job, as the module `nori:Job`.
pub const JOB_SCHEMA_PKL: &str = include_str!("../schemas/Job.pkl");
/// The Pkl schema of a monitor, as the module `nori:Monitor`.
pub const MONITOR_SCHEMA_PKL: &str = include_str!("../schemas/Monitor.pkl");
/// The Pkl schema of a plugin, as the module `nori:Plugin`.
pub const PLUGIN_SCHEMA_PKL: &str = include_str!("../schemas/Plugin.pkl");
/// The Pkl schema of a gate, as the module `nori:Gate`.
pub const GATE_SCHEMA_PKL: &str = include_str!("../schemas/Gate.pkl");

/// Every runtime schema keyed by the URIs a module can import it as: `nori:Job` and
/// `nori:Job.pkl`, and likewise for `Monitor`, `Plugin` and `Gate`.
pub fn runtime_schemas() -> BTreeMap<String, String> {
    [
        ("Job", JOB_SCHEMA_PKL),
        ("Monitor", MONITOR_SCHEMA_PKL),
        ("Plugin", PLUGIN_SCHEMA_PKL),
        ("Gate", GATE_SCHEMA_PKL),
    ]
    .into_iter()
    .flat_map(|(name, source)| {
        [
            (format!("nori:{name}"), source.to_owned()),
            (format!("nori:{name}.pkl"), source.to_owned()),
        ]
    })
    .collect()
}

impl EvaluatorOptions {
    /// Adds the `nori:Job`, `nori:Monitor`, `nori:Plugin` and `nori:Gate` schemas to the virtual modules
    /// already set, so a module can `amends` them.
    pub fn with_runtime_schemas(mut self) -> Self {
        self.virtual_modules.extend(runtime_schemas());
        self
    }
}
