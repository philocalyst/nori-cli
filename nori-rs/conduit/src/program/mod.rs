//! Programs: declarative multi-actor workflows written in Pkl.
//!
//! A program is a Pkl module that `amends "nori:Program"`: it declares actors, worlds, tools
//! and a tree of typed steps. [`load_source`] evaluates it with the native evaluator,
//! deserializes the typed [`Program`](model::Program), checks it and compiles it to an op
//! graph ([`CompiledProgram`]). Nothing runs at load time; [`crate::run`] executes the graph.

mod compile;
mod locate;
pub mod model;
mod skill;
pub mod template;

use std::collections::BTreeMap;
use std::path::Path;

use nori_pkl::native::parse_document;

use crate::error::ConduitError;
use crate::op::CompiledProgram;
use crate::schemas::program_evaluator_options;
pub use compile::compile;
pub use locate::Locator;

/// Whether `source` is a program module (it amends `nori:Program`).
pub fn is_program(source: &str) -> bool {
    parse_document(source).ok().is_some_and(|document| {
        matches!(
            document.module().amends.as_deref(),
            Some("nori:Program" | "nori:Program.pkl" | "nori:schemas/Program.pkl")
        )
    })
}

/// Evaluate, check and compile a program. `path` is the program's file (its directory anchors
/// relative imports and wasm module paths); `properties` are the captured machine properties
/// readable as `read("prop:...")`.
pub fn load_source(
    source: &str,
    path: Option<&Path>,
    properties: BTreeMap<String, String>,
) -> Result<CompiledProgram, ConduitError> {
    load_source_with(source, path, properties, &BTreeMap::new())
}

/// [`load_source`] for a program that may be a skill (`nori:Skill`): `arguments` are the values of
/// its `parameters`, put in place of each `{{parameter}}` before the program is checked, so a
/// skill run with them is an ordinary program. Unknown arguments, and required parameters with no
/// argument and no default, are errors that name them.
pub fn load_source_with(
    source: &str,
    path: Option<&Path>,
    properties: BTreeMap<String, String>,
    arguments: &BTreeMap<String, String>,
) -> Result<CompiledProgram, ConduitError> {
    let file = path.map_or_else(|| "<program>".to_owned(), |path| path.display().to_string());
    // The arguments are also visible to the module while it evaluates, as `read?("prop:skill.<name>")`
    // (null when absent), so a skill can shape its actors and steps from them and not only its text.
    let mut properties = properties;
    properties.extend(
        arguments
            .iter()
            .map(|(name, value)| (format!("skill.{name}"), value.clone())),
    );
    let options = program_evaluator_options().properties(properties);
    let mut value: serde_json::Value =
        nori_pkl::native::evaluate_source_at_path(source, path, &options)
            .map_err(|message| ConduitError::Compilation { message })?;
    skill::apply_arguments(&mut value, arguments, &file)?;
    if value.get("steps").is_none() && value.get("name").is_none() {
        return Err(ConduitError::Compilation {
            message: format!(
                "{file} does not evaluate to a program; start it with `amends \"nori:Program\"`"
            ),
        });
    }
    let program: model::Program =
        serde_json::from_value(value).map_err(|error| ConduitError::Compilation {
            message: format!("{file} is not a valid program: {error}"),
        })?;
    compile(&program, &Locator::new(&file, source))
}

/// [`load_source`] for a program file.
pub fn load_file(
    path: &Path,
    properties: BTreeMap<String, String>,
) -> Result<CompiledProgram, ConduitError> {
    let source = std::fs::read_to_string(path).map_err(|error| ConduitError::Compilation {
        message: format!("cannot read {}: {error}", path.display()),
    })?;
    load_source(&source, Some(path), properties)
}
