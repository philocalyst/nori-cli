//! Finding files by name and by content, honoring `.gitignore`.

use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

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
use super::string_arg;

/// Files a glob returns.
const MAX_GLOB_RESULTS: usize = 100;
/// Matching lines a grep returns.
const MAX_GREP_MATCHES: usize = 100;
/// Files larger than this are not searched.
const MAX_GREP_FILE_BYTES: u64 = 4 * 1024 * 1024;
/// Characters of a matching line shown.
const MAX_GREP_LINE_CHARS: usize = 400;

fn walk(root: &Path) -> impl Iterator<Item = ignore::DirEntry> {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|kind| kind.is_file()))
}

fn newest_first(mut files: Vec<(SystemTime, PathBuf)>) -> Vec<PathBuf> {
    files.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    files.into_iter().map(|(_, path)| path).collect()
}

fn mtime(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn search_root(input: &Value, context: &ToolContext) -> PathBuf {
    input["path"]
        .as_str()
        .filter(|path| !path.is_empty())
        .map_or_else(|| context.cwd.clone(), |raw| paths::resolve(raw, context))
}

/// `glob`: files whose path matches a pattern, newest first.
pub struct Glob {
    pub name: String,
    pub description: String,
}

impl Default for Glob {
    fn default() -> Self {
        Self {
            name: "glob".to_owned(),
            description: "Find files by name with a glob pattern such as `**/*.rs` or `src/**/test_*.py`, under `path` (default: the working directory), honoring .gitignore. Returns up to 100 paths, most recently modified first. Use it instead of `find` or `ls` in the shell.".to_owned(),
        }
    }
}

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "The glob pattern"},
                    "path": {"type": "string", "description": "The directory to search in"},
                },
                "required": ["pattern"],
            }),
        }
    }

    async fn prepare(&self, input: &Value, context: &ToolContext) -> Result<Invocation, String> {
        let pattern = string_arg(input, "pattern")?;
        let root = search_root(input, context);
        let mut invocation = Invocation::new(format!("Find {pattern}"), ToolKind::Search);
        invocation.checks = paths::checks("glob", &root, context);
        Ok(invocation)
    }

    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput {
        let Ok(pattern) = string_arg(&input, "pattern").map(ToOwned::to_owned) else {
            return ToolOutput::error("The `pattern` argument is required.");
        };
        let root = search_root(&input, context);
        let found = blocking(move || -> Result<String, String> {
            let matcher = globset::GlobBuilder::new(&pattern)
                .literal_separator(false)
                .build()
                .map_err(|error| format!("Bad glob pattern {pattern:?}: {error}"))?
                .compile_matcher();
            let files: Vec<(SystemTime, PathBuf)> = walk(&root)
                .filter(|entry| {
                    let relative = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                    matcher.is_match(relative)
                })
                .map(|entry| (mtime(entry.path()), entry.into_path()))
                .collect();
            let total = files.len();
            let files = newest_first(files);
            if files.is_empty() {
                return Ok("No files found.".to_owned());
            }
            let mut out = files
                .iter()
                .take(MAX_GLOB_RESULTS)
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            if total > MAX_GLOB_RESULTS {
                out.push_str(&format!(
                    "\n({total} files matched; showing the {MAX_GLOB_RESULTS} newest. Narrow the pattern.)"
                ));
            }
            Ok(out)
        })
        .await;
        match found {
            Ok(Ok(text)) => ToolOutput::text(text),
            Ok(Err(error)) | Err(error) => ToolOutput::error(error),
        }
    }
}

/// `grep`: lines matching a regular expression.
pub struct Grep {
    pub name: String,
    pub description: String,
}

impl Default for Grep {
    fn default() -> Self {
        Self {
            name: "grep".to_owned(),
            description: "Search file contents with a regular expression (Rust regex syntax, e.g. `fn\\s+main`, `log.*Error`) under `path` (default: the working directory), honoring .gitignore. `include` limits the files searched with a glob such as `*.ts` or `*.{ts,tsx}`. Returns up to 100 matching lines with their file and line number, files most recently modified first. Use it instead of `grep` or `rg` in the shell.".to_owned(),
        }
    }
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "The regular expression"},
                    "path": {"type": "string", "description": "The directory to search in"},
                    "include": {"type": "string", "description": "A glob for the files to search"},
                },
                "required": ["pattern"],
            }),
        }
    }

    async fn prepare(&self, input: &Value, context: &ToolContext) -> Result<Invocation, String> {
        let pattern = string_arg(input, "pattern")?;
        regex::Regex::new(pattern)
            .map_err(|error| format!("Bad regular expression {pattern:?}: {error}"))?;
        let root = search_root(input, context);
        let mut invocation = Invocation::new(format!("Search {pattern}"), ToolKind::Search);
        invocation.checks = paths::checks("grep", &root, context);
        Ok(invocation)
    }

    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput {
        let Ok(pattern) = string_arg(&input, "pattern").map(ToOwned::to_owned) else {
            return ToolOutput::error("The `pattern` argument is required.");
        };
        let include = input["include"].as_str().map(ToOwned::to_owned);
        let root = search_root(&input, context);
        let cancel = context.cancel.clone();
        let found = blocking(move || grep(&root, &pattern, include.as_deref(), &cancel)).await;
        match found {
            Ok(Ok(text)) => ToolOutput::text(text),
            Ok(Err(error)) | Err(error) => ToolOutput::error(error),
        }
    }
}

/// A file's matching lines (number and text), and when it was modified.
type FileHits = (SystemTime, PathBuf, Vec<(usize, String)>);

fn grep(
    root: &Path,
    pattern: &str,
    include: Option<&str>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String, String> {
    let regex = regex::Regex::new(pattern)
        .map_err(|error| format!("Bad regular expression {pattern:?}: {error}"))?;
    let include = include
        .map(|glob| {
            globset::Glob::new(glob)
                .map(|glob| glob.compile_matcher())
                .map_err(|error| format!("Bad include glob {glob:?}: {error}"))
        })
        .transpose()?;
    let mut hits: Vec<FileHits> = Vec::new();
    let mut total = 0;
    for entry in walk(root) {
        if cancel.is_cancelled() {
            return Err("Cancelled.".to_owned());
        }
        let path = entry.path();
        if let Some(include) = &include {
            let name = path.file_name().map(Path::new).unwrap_or(path);
            let relative = path.strip_prefix(root).unwrap_or(path);
            if !include.is_match(name) && !include.is_match(relative) {
                continue;
            }
        }
        if entry
            .metadata()
            .is_ok_and(|metadata| metadata.len() > MAX_GREP_FILE_BYTES)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.iter().take(8192).any(|byte| *byte == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<(usize, String)> = text
            .lines()
            .enumerate()
            .filter(|(_, line)| regex.is_match(line))
            .map(|(index, line)| (index + 1, line.chars().take(MAX_GREP_LINE_CHARS).collect()))
            .collect();
        if !lines.is_empty() {
            total += lines.len();
            hits.push((mtime(path), path.to_path_buf(), lines));
        }
    }
    if hits.is_empty() {
        return Ok("No matches found.".to_owned());
    }
    hits.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    let mut out = String::new();
    let mut shown = 0;
    for (_, path, lines) in hits {
        if shown >= MAX_GREP_MATCHES {
            break;
        }
        out.push_str(&format!("{}:\n", path.display()));
        for (number, line) in lines {
            if shown >= MAX_GREP_MATCHES {
                break;
            }
            out.push_str(&format!("  {number}: {line}\n"));
            shown += 1;
        }
    }
    if total > MAX_GREP_MATCHES {
        out.push_str(&format!(
            "({total} matching lines; showing {MAX_GREP_MATCHES}. Narrow the pattern or the path.)\n"
        ));
    }
    Ok(out)
}
