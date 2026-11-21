//! `list`: a directory's tree.

use std::path::Path;
use std::path::PathBuf;

use async_trait::async_trait;
use nori_acp_kit::schema::ToolKind;
use nori_llm::ToolSpec;
use serde_json::Value;
use serde_json::json;

use super::Invocation;
use super::Tool;
use super::ToolContext;
use super::ToolOutput;
use super::blocking;
use super::paths;

/// Entries a listing returns.
const MAX_LIST_ENTRIES: usize = 200;

/// `list`: a directory's tree.
pub struct List {
    pub name: String,
    pub description: String,
}

impl Default for List {
    fn default() -> Self {
        Self {
            name: "list".to_owned(),
            description: "List files and directories under `path` (default: the working directory) as a tree, honoring .gitignore. `ignore` adds glob patterns to leave out. Prefer `glob` and `grep` when you know what you are looking for.".to_owned(),
        }
    }
}

#[async_trait]
impl Tool for List {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "The directory to list"},
                    "ignore": {"type": "array", "items": {"type": "string"}, "description": "Glob patterns to leave out"},
                },
            }),
        }
    }

    async fn prepare(&self, input: &Value, context: &ToolContext) -> Result<Invocation, String> {
        let path = input["path"]
            .as_str()
            .map_or_else(|| context.cwd.clone(), |raw| paths::resolve(raw, context));
        let mut invocation = Invocation::new(
            format!("List {}", paths::display(&path, context)),
            ToolKind::Search,
        );
        invocation.checks = paths::checks("list", &path, context);
        invocation.locations = vec![path];
        Ok(invocation)
    }

    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput {
        let root = input["path"]
            .as_str()
            .map_or_else(|| context.cwd.clone(), |raw| paths::resolve(raw, context));
        let ignore: Vec<String> = input["ignore"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect();
        match blocking(move || list(&root, &ignore)).await {
            Ok(Ok(text)) => ToolOutput::text(text),
            Ok(Err(error)) | Err(error) => ToolOutput::error(error),
        }
    }
}

fn list(root: &Path, ignore: &[String]) -> Result<String, String> {
    if !root.is_dir() {
        return Err(format!("{} is not a directory.", root.display()));
    }
    let mut overrides = ignore::overrides::OverrideBuilder::new(root);
    for pattern in ignore {
        overrides
            .add(&format!("!{pattern}"))
            .map_err(|error| format!("Bad ignore pattern {pattern:?}: {error}"))?;
    }
    let overrides = overrides.build().map_err(|error| error.to_string())?;
    let mut entries: Vec<PathBuf> = Vec::new();
    let mut truncated = false;
    for entry in ignore::WalkBuilder::new(root)
        .overrides(overrides)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_path(Ord::cmp)
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= MAX_LIST_ENTRIES {
            truncated = true;
            break;
        }
        entries.push(entry.path().to_path_buf());
    }
    let mut out = format!("{}/\n", root.display());
    for path in &entries {
        let relative = path.strip_prefix(root).unwrap_or(path);
        let depth = relative.components().count();
        let name = relative
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let slash = if path.is_dir() { "/" } else { "" };
        out.push_str(&format!("{}{name}{slash}\n", "  ".repeat(depth)));
    }
    if truncated {
        out.push_str(&format!(
            "(Showing the first {MAX_LIST_ENTRIES} entries. List a subdirectory or use glob.)\n"
        ));
    }
    Ok(out)
}
