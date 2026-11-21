//! The agent-facing LSP tools as data plus one dispatcher, so the shared tool crate and the
//! host-tool layer can register them without knowing anything about language servers:
//!
//! ```ignore
//! for spec in nori_lsp::tools::specs() { register(spec.name, spec.description, spec.input_schema) }
//! let output = nori_lsp::tools::call(&manager, name, arguments).await;
//! ```

mod format;
mod specs;

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::Value;

use crate::error::LspError;
use crate::error::LspResult;
use crate::manager::LspManager;
use crate::render::RenderOptions;
use crate::render::display_path;
use crate::render::render_file_list;
use crate::types::CallDirection;
use crate::types::GotoKind;
use crate::types::ServerState;
use crate::types::Target;

/// One tool: what to call it, how to describe it to the model and the schema of its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    /// A JSON Schema object.
    pub input_schema: Value,
}

/// What a tool call returns to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspToolOutput {
    pub text: String,
    pub is_error: bool,
}

impl LspToolOutput {
    fn ok(text: String) -> Self {
        Self {
            text,
            is_error: false,
        }
    }

    fn error(text: String) -> Self {
        Self {
            text,
            is_error: true,
        }
    }
}

/// The names of the tools [`specs`] returns.
pub const TOOL_NAMES: [&str; 7] = [
    "lsp_diagnostics",
    "lsp_definition",
    "lsp_references",
    "lsp_hover",
    "lsp_symbols",
    "lsp_rename",
    "lsp_call_hierarchy",
];

pub fn specs() -> Vec<ToolSpec> {
    specs::all()
}

#[derive(Deserialize)]
struct At {
    path: PathBuf,
    line: i32,
    column: Option<i32>,
    symbol: Option<String>,
}

impl At {
    fn target(self) -> Target {
        Target {
            path: self.path,
            line: self.line,
            column: self.column,
            symbol: self.symbol,
        }
    }
}

#[derive(Deserialize)]
struct DiagnosticsArgs {
    path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct DefinitionArgs {
    #[serde(flatten)]
    at: At,
    kind: Option<GotoKind>,
}

#[derive(Deserialize)]
struct ReferencesArgs {
    #[serde(flatten)]
    at: At,
    include_declaration: Option<bool>,
    limit: Option<i32>,
}

#[derive(Deserialize)]
struct SymbolsArgs {
    path: Option<PathBuf>,
    query: Option<String>,
    limit: Option<i32>,
}

#[derive(Deserialize)]
struct RenameArgs {
    #[serde(flatten)]
    at: At,
    new_name: String,
}

#[derive(Deserialize)]
struct CallsArgs {
    #[serde(flatten)]
    at: At,
    direction: CallDirection,
}

fn arguments<T: serde::de::DeserializeOwned>(name: &str, args: Value) -> LspResult<T> {
    serde_json::from_value(args).map_err(|error| LspError::InvalidPosition {
        message: format!("invalid arguments for {name}: {error}"),
    })
}

fn limit(value: Option<i32>, default: usize) -> usize {
    value
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// Runs one tool. Unknown names and bad arguments come back as errors the model can read.
pub async fn call(manager: &LspManager, name: &str, args: Value) -> LspToolOutput {
    match run(manager, name, args).await {
        Ok(text) => LspToolOutput::ok(text),
        Err(error) => LspToolOutput::error(error.to_string()),
    }
}

async fn run(manager: &LspManager, name: &str, args: Value) -> LspResult<String> {
    let root = manager.workspace_root().to_path_buf();
    match name {
        "lsp_diagnostics" => {
            let args: DiagnosticsArgs = arguments(name, args)?;
            diagnostics(manager, args.path, &root).await
        }
        "lsp_definition" => {
            let args: DefinitionArgs = arguments(name, args)?;
            let kind = args.kind.unwrap_or(GotoKind::Definition);
            let answer = manager.goto(kind, &args.at.target()).await?;
            Ok(format::locations(&answer, kind.label(), 20, &root))
        }
        "lsp_references" => {
            let args: ReferencesArgs = arguments(name, args)?;
            let include = args.include_declaration.unwrap_or(true);
            let answer = manager.references(&args.at.target(), include).await?;
            Ok(format::references(&answer, limit(args.limit, 50), &root))
        }
        "lsp_hover" => {
            let args: At = arguments(name, args)?;
            let answer = manager.hover(&args.target()).await?;
            Ok(format::hover(&answer, &root))
        }
        "lsp_symbols" => {
            let args: SymbolsArgs = arguments(name, args)?;
            symbols(manager, args, &root).await
        }
        "lsp_rename" => {
            let args: RenameArgs = arguments(name, args)?;
            let answer = manager.rename(&args.at.target(), &args.new_name).await?;
            Ok(format::rename(&answer, &args.new_name, &root))
        }
        "lsp_call_hierarchy" => {
            let args: CallsArgs = arguments(name, args)?;
            let incoming = args.direction == CallDirection::Incoming;
            let answer = manager
                .call_hierarchy(&args.at.target(), args.direction)
                .await?;
            Ok(format::calls(&answer, incoming, 40, &root))
        }
        other => Err(LspError::InvalidPosition {
            message: format!(
                "unknown LSP tool `{other}`; available: {}",
                TOOL_NAMES.join(", ")
            ),
        }),
    }
}

async fn symbols(
    manager: &LspManager,
    args: SymbolsArgs,
    root: &std::path::Path,
) -> LspResult<String> {
    let cap = limit(args.limit, 100);
    match (
        args.query.filter(|query| !query.trim().is_empty()),
        args.path,
    ) {
        (Some(query), near) => {
            let found = manager.workspace_symbols(&query, near.as_deref()).await?;
            Ok(format::symbols(
                &found,
                &format!("matching `{query}`"),
                true,
                cap,
                root,
            ))
        }
        (None, Some(path)) => {
            let found = manager.document_symbols(&path).await?;
            let what = format!("in {}", display_path(&path, root));
            Ok(format::symbols(&found, &what, false, cap, root))
        }
        (None, None) => Err(LspError::InvalidPosition {
            message: "give `path` for a file outline, or `query` to search the project".to_owned(),
        }),
    }
}

async fn diagnostics(
    manager: &LspManager,
    path: Option<PathBuf>,
    root: &std::path::Path,
) -> LspResult<String> {
    let options = RenderOptions::new(root);
    if let Some(path) = path {
        let report = manager.refresh_diagnostics(&path).await;
        let note = match &report.status {
            crate::diagnostics::ReportStatus::NoServer => {
                return Err(LspError::NoServer {
                    path,
                    hint: manager.registry().install_hint(
                        crate::registry::language_for_path(&report.path).unwrap_or(""),
                    ),
                });
            }
            crate::diagnostics::ReportStatus::Failed(reason) => {
                return Err(LspError::ServerUnavailable {
                    server: report.servers.join(", "),
                    reason: reason.clone(),
                });
            }
            crate::diagnostics::ReportStatus::Stale => {
                "\n(the language server had not finished analysing; try again shortly)"
            }
            crate::diagnostics::ReportStatus::Fresh => "",
        };
        return Ok(format!(
            "{}{note}",
            render_file_list(&report.path, &report.file, &options)
        ));
    }

    let files = manager.project_errors();
    let mut out = String::new();
    if files.is_empty() {
        out.push_str("No errors reported by any running language server.\n");
    } else {
        let total: usize = files.iter().map(|file| file.diagnostics.len()).sum();
        out.push_str(&format!("{total} error(s) in {} file(s):\n", files.len()));
        for file in files.iter().take(15) {
            out.push_str(&render_file_list(&file.path, &file.diagnostics, &options));
            out.push('\n');
        }
        if files.len() > 15 {
            out.push_str(&format!("... {} more files\n", files.len() - 15));
        }
    }
    let status = manager.status().await;
    if status.is_empty() {
        out.push_str(
            "No language server is running yet; they start when a source file is read or edited.",
        );
    } else {
        out.push_str("Language servers:\n");
        for server in status {
            let state = match server.state {
                ServerState::Starting => "starting".to_owned(),
                ServerState::Ready { busy: true } => "running (still indexing)".to_owned(),
                ServerState::Ready { busy: false } => "running".to_owned(),
                ServerState::Failed { reason, .. } => format!("failed: {reason}"),
            };
            out.push_str(&format!(
                "  {} ({}): {state}\n",
                server.name,
                display_path(&server.root, root)
            ));
        }
    }
    Ok(out.trim_end().to_owned())
}
