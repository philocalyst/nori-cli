//! The part of a skill module the catalog reads.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use super::check_name;
use super::finish;

/// One parameter of a skill.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillParameter {
    /// What the argument is.
    #[serde(default)]
    pub description: String,
    /// The value used when the caller gives none.
    #[serde(default)]
    pub default: Option<String>,
    /// Whether the caller must give a value.
    #[serde(default = "required_by_default")]
    pub required: bool,
}

fn required_by_default() -> bool {
    true
}

/// What the catalog reads from a skill module without understanding its program: the name the
/// skill is run by, its summary and its parameters. Fields it does not know (the program: actors,
/// steps, result) are ignored; `nori-conduit` owns them and supplies the `nori:Skill` schema.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillMeta {
    /// The skill's name (`[a-z0-9][a-z0-9_-]*`); `/name` runs it.
    pub name: String,
    /// What the skill does, in a sentence.
    #[serde(default)]
    pub summary: String,
    /// The arguments it takes, by name.
    #[serde(default)]
    pub parameters: BTreeMap<String, SkillParameter>,
    /// The slash command that runs it, when it is not `/name`.
    #[serde(default)]
    pub command: Option<String>,
}

impl SkillMeta {
    /// Checks the name. Every message names the field.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        check_name("name", &self.name, &mut problems);
        finish(problems)
    }
}
