//! Native Rust evaluation and lossless syntax for Nori's Pkl modules.
//!
//! Unsupported Pkl constructs fail explicitly; this runtime never launches a
//! subprocess. Journal replay should inspect syntax, never evaluate programs.
mod diagnostics;
pub mod edit;
mod evaluator;
pub mod source;

use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::path::Path;

pub use source::Document;
pub use source::Entry;
pub use source::Expr;
pub use source::Module;
pub use source::Property;
pub use source::PropertySource;
pub use source::parse_document;

/// Explicit capabilities and captured observations available during evaluation.
#[derive(Clone, Debug)]
pub struct EvaluatorOptions {
    pub(crate) allowed_modules: Vec<String>,
    pub(crate) allowed_resources: Vec<String>,
    pub(crate) properties: BTreeMap<String, String>,
    pub(crate) virtual_modules: BTreeMap<String, String>,
    pub(crate) environment: Option<BTreeMap<String, String>>,
    pub(crate) max_depth: i64,
}

impl Default for EvaluatorOptions {
    fn default() -> Self {
        Self::preconfigured()
    }
}

impl EvaluatorOptions {
    pub fn new() -> Self {
        Self {
            allowed_modules: Vec::new(),
            allowed_resources: Vec::new(),
            properties: BTreeMap::new(),
            virtual_modules: BTreeMap::new(),
            environment: Some(BTreeMap::new()),
            max_depth: 256,
        }
    }
    pub fn preconfigured() -> Self {
        Self {
            allowed_modules: vec![
                "pkl:".into(),
                "nori:".into(),
                "file:".into(),
                "repl:".into(),
                "package:".into(),
                "projectpackage:".into(),
                "https:".into(),
            ],
            allowed_resources: vec![
                "prop:".into(),
                "env:".into(),
                "file:".into(),
                "package:".into(),
                "projectpackage:".into(),
                "https:".into(),
            ],
            properties: BTreeMap::new(),
            virtual_modules: BTreeMap::new(),
            environment: None,
            max_depth: 256,
        }
    }
    pub fn allowed_modules(mut self, patterns: Vec<String>) -> Self {
        self.allowed_modules = patterns;
        self
    }
    pub fn allowed_resources(mut self, patterns: Vec<String>) -> Self {
        self.allowed_resources = patterns;
        self
    }
    pub fn properties(mut self, properties: BTreeMap<String, String>) -> Self {
        self.properties = properties;
        self
    }
    pub fn virtual_modules(mut self, modules: BTreeMap<String, String>) -> Self {
        self.virtual_modules = modules;
        self
    }
    pub fn virtual_module(mut self, uri: impl Into<String>, source: impl Into<String>) -> Self {
        self.virtual_modules.insert(uri.into(), source.into());
        self
    }
    pub fn environment(mut self, environment: BTreeMap<String, String>) -> Self {
        self.environment = Some(environment);
        self
    }
    pub fn max_depth(mut self, max_depth: i64) -> Self {
        self.max_depth = max_depth;
        self
    }
}

pub fn evaluate<T: DeserializeOwned>(
    source: &str,
    options: &EvaluatorOptions,
) -> Result<T, String> {
    evaluate_source_at_path(source, None, options)
}

pub fn evaluate_path<T: DeserializeOwned>(
    path: &Path,
    options: &EvaluatorOptions,
) -> Result<T, String> {
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    evaluate_source_at_path(&source, Some(path), options)
}

pub fn evaluate_source_at_path<T: DeserializeOwned>(
    source: &str,
    path: Option<&Path>,
    options: &EvaluatorOptions,
) -> Result<T, String> {
    let value = evaluator::evaluate(source, path, options)?;
    serde_json::from_value(value).map_err(|error| format!("invalid module data: {error}"))
}

#[cfg(test)]
mod tests;
