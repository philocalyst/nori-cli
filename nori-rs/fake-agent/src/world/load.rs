//! Reading worlds from Pkl and JSON.

use std::path::Path;

use nori_pkl::EvaluatorOptions;

use super::World;

/// The Pkl schema of a world, as the module `nori:World`.
pub const WORLD_SCHEMA_PKL: &str = include_str!("../../schema/World.pkl");
/// Ready-made agents and step helpers for worlds, as the module `nori:Kit`.
pub const KIT_PKL: &str = include_str!("../../schema/Kit.pkl");

#[derive(Debug, thiserror::Error)]
pub enum WorldError {
    #[error("cannot read world {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("world {origin} does not evaluate: {message}")]
    Pkl { origin: String, message: String },
    #[error("world {origin} is not a valid world: {message}")]
    Shape { origin: String, message: String },
    #[error("world {origin} is invalid: {message}")]
    Invalid { origin: String, message: String },
}

fn options() -> EvaluatorOptions {
    let mut modules = std::collections::BTreeMap::new();
    for name in ["nori:World", "nori:World.pkl", "nori:schemas/World.pkl"] {
        modules.insert(name.to_owned(), WORLD_SCHEMA_PKL.to_owned());
    }
    for name in ["nori:Kit", "nori:Kit.pkl"] {
        modules.insert(name.to_owned(), KIT_PKL.to_owned());
    }
    EvaluatorOptions::preconfigured().virtual_modules(modules)
}

impl World {
    /// A world from a file: Pkl for `.pkl`, JSON for `.json`; anything else is tried as JSON and
    /// then as Pkl.
    pub fn from_path(path: &Path) -> Result<Self, WorldError> {
        let origin = path.display().to_string();
        let source = std::fs::read_to_string(path).map_err(|source| WorldError::Read {
            path: origin.clone(),
            source,
        })?;
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("pkl") => Self::from_pkl(&source, Some(path)),
            Some("json") => Self::from_json(&source, &origin),
            _ => Self::from_json(&source, &origin).or_else(|_| Self::from_pkl(&source, Some(path))),
        }
    }

    /// A world from Pkl source that `amends "nori:World"`. `path` anchors relative imports.
    pub fn from_pkl(source: &str, path: Option<&Path>) -> Result<Self, WorldError> {
        let origin = path.map_or_else(|| "<pkl>".to_owned(), |path| path.display().to_string());
        let value: serde_json::Value =
            nori_pkl::native::evaluate_source_at_path(source, path, &options()).map_err(
                |message| WorldError::Pkl {
                    origin: origin.clone(),
                    message,
                },
            )?;
        Self::from_value(value, &origin)
    }

    /// A world from JSON, such as a generated fixture.
    pub fn from_json(source: &str, origin: &str) -> Result<Self, WorldError> {
        let value = serde_json::from_str(source).map_err(|error| WorldError::Shape {
            origin: origin.to_owned(),
            message: error.to_string(),
        })?;
        Self::from_value(value, origin)
    }

    fn from_value(value: serde_json::Value, origin: &str) -> Result<Self, WorldError> {
        let world: Self = serde_json::from_value(value).map_err(|error| WorldError::Shape {
            origin: origin.to_owned(),
            message: error.to_string(),
        })?;
        world.validate().map_err(|message| WorldError::Invalid {
            origin: origin.to_owned(),
            message,
        })?;
        Ok(world)
    }
}
