//! The tool catalog as data: names, aliases, descriptions and input schemas.
//!
//! A host registers [`catalog`] with its model API (or MCP server) and forwards each call to
//! [`Workspace::call`](crate::Workspace::call), which accepts the canonical name or any alias.

mod descriptions;
mod schemas;

use serde_json::Value;

/// How a tool's input is sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolFormat {
    /// A JSON object matching `input_schema`.
    Json,
    /// Raw text constrained by a grammar (OpenAI custom tools). Hosts without freeform support
    /// send `{"input": "<text>"}` using `input_schema` instead.
    Freeform {
        syntax: &'static str,
        definition: &'static str,
    },
}

/// One tool, in the shape of an MCP `tools/list` entry plus aliases.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    /// JSON Schema of the arguments: always an object schema.
    pub input_schema: Value,
    /// Other names the same tool answers to (other harnesses' spellings).
    pub aliases: Vec<&'static str>,
    pub format: ToolFormat,
    /// The tool never changes a file.
    pub read_only: bool,
}

/// Which edit tools a model is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditStyle {
    /// `read`, `write`, `edit`, `multi_edit`.
    #[default]
    Replace,
    /// `read`, `apply_patch`: for models trained on the Codex patch format.
    Patch,
    /// All five.
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Read,
    Write,
    Edit,
    MultiEdit,
    ApplyPatch,
}

fn key(name: &str) -> String {
    let name = name.rsplit("__").next().unwrap_or(name);
    name.chars()
        .filter(|c| *c != '_' && *c != '-' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

impl ToolKind {
    pub const ALL: [Self; 5] = [
        Self::Read,
        Self::Write,
        Self::Edit,
        Self::MultiEdit,
        Self::ApplyPatch,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Edit => "edit",
            Self::MultiEdit => "multi_edit",
            Self::ApplyPatch => "apply_patch",
        }
    }

    pub fn aliases(self) -> &'static [&'static str] {
        match self {
            Self::Read => &["Read", "read_file", "view", "view_file", "open_file", "cat"],
            Self::Write => &[
                "Write",
                "write_file",
                "write_to_file",
                "create_file",
                "create",
            ],
            Self::Edit => &[
                "Edit",
                "str_replace",
                "edit_file",
                "replace",
                "replace_in_file",
                "search_replace",
            ],
            Self::MultiEdit => &["MultiEdit", "multiedit"],
            Self::ApplyPatch => &["patch", "applypatch"],
        }
    }

    /// The tool a name (canonical, alias, any case or separators, with or without an
    /// `mcp__server__` prefix) refers to.
    pub fn resolve(name: &str) -> Option<Self> {
        let wanted = key(name);
        Self::ALL.into_iter().find(|kind| {
            key(kind.name()) == wanted || kind.aliases().iter().any(|alias| key(alias) == wanted)
        })
    }

    pub fn spec(self) -> ToolSpec {
        let (description, format) = match self {
            Self::Read => (descriptions::READ, ToolFormat::Json),
            Self::Write => (descriptions::WRITE, ToolFormat::Json),
            Self::Edit => (descriptions::EDIT, ToolFormat::Json),
            Self::MultiEdit => (descriptions::MULTI_EDIT, ToolFormat::Json),
            Self::ApplyPatch => (
                descriptions::APPLY_PATCH,
                ToolFormat::Freeform {
                    syntax: "lark",
                    definition: descriptions::APPLY_PATCH_GRAMMAR,
                },
            ),
        };
        ToolSpec {
            name: self.name(),
            description: description.to_owned(),
            input_schema: schemas::input_schema(self),
            aliases: self.aliases().to_vec(),
            format,
            read_only: self == Self::Read,
        }
    }
}

/// The specs for `style`, in a stable order.
pub fn catalog(style: EditStyle) -> Vec<ToolSpec> {
    let kinds: &[ToolKind] = match style {
        EditStyle::Replace => &[
            ToolKind::Read,
            ToolKind::Write,
            ToolKind::Edit,
            ToolKind::MultiEdit,
        ],
        EditStyle::Patch => &[ToolKind::Read, ToolKind::ApplyPatch],
        EditStyle::Both => &ToolKind::ALL,
    };
    kinds.iter().map(|kind| kind.spec()).collect()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn names_aliases_and_prefixes_resolve() {
        for (name, kind) in [
            ("read", ToolKind::Read),
            ("Read", ToolKind::Read),
            ("mcp__nori__edit", ToolKind::Edit),
            ("str_replace", ToolKind::Edit),
            ("MultiEdit", ToolKind::MultiEdit),
            ("multi-edit", ToolKind::MultiEdit),
            ("write_file", ToolKind::Write),
            ("apply-patch", ToolKind::ApplyPatch),
        ] {
            assert_eq!(ToolKind::resolve(name), Some(kind), "{name}");
        }
        assert_eq!(ToolKind::resolve("bash"), None);
    }

    #[test]
    fn catalog_schemas_are_object_schemas_and_names_are_unique() {
        let specs = catalog(EditStyle::Both);
        assert_eq!(specs.len(), 5);
        for spec in &specs {
            assert_eq!(spec.input_schema["type"], "object", "{}", spec.name);
        }
        let mut names: Vec<_> = specs.iter().map(|spec| spec.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 5);
    }
}
