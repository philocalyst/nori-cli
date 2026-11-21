//! The tools the engine runs for a model.
//!
//! A [`Tool`] says what it is ([`Tool::spec`]), what a call is about before it runs
//! ([`Tool::prepare`]: the card's title and kind, the paths it touches, the permission checks it
//! needs, a preview), and runs. The engine does everything around it: the ACP tool call, the
//! permission question, cancellation, the result the model reads.

mod host;
mod image;
mod list;
mod paths;
mod plan;
mod search;
mod shared;
mod shell;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use nori_acp_kit::schema::Meta;
use nori_acp_kit::schema::PlanEntry;
use nori_acp_kit::schema::ToolCallContent;
use nori_acp_kit::schema::ToolKind;
use nori_llm::ToolResultPart;
use nori_llm::ToolSpec;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

pub use host::HostTool;
pub use image::ViewImage;
pub use list::List;
pub use nori_tools::EditStyle;
pub use nori_tools::WorkspaceConfig;
pub use plan::Todo;
pub use plan::TodoStyle;
pub use search::Glob;
pub use search::Grep;
pub use shared::SharedTool;
pub use shell::Shell;
pub use shell::ShellStyle;

/// What one session's tools share.
#[derive(Clone)]
pub struct ToolContext {
    pub cwd: PathBuf,
    /// Where the session may work without asking about an outside directory: the working
    /// directory and the additional directories.
    pub roots: Vec<PathBuf>,
    pub cancel: CancellationToken,
    /// The session's file tools (`nori-tools`): its read ledger is what lets an edit refuse a
    /// file that changed since the model saw it.
    pub files: Arc<nori_tools::Workspace>,
    /// The session's own shell, for when Nori does not lend its `bash`.
    pub shell: Arc<tokio::sync::Mutex<Option<nori_bash::BashSession>>>,
    /// Whether the model reads images.
    pub images: bool,
}

impl ToolContext {
    pub fn new(cwd: PathBuf, roots: Vec<PathBuf>, mut files: nori_tools::WorkspaceConfig) -> Self {
        files
            .roots
            .extend(roots.iter().filter(|root| **root != cwd).cloned());
        Self {
            files: Arc::new(nori_tools::Workspace::new(
                cwd.clone(),
                Arc::new(nori_tools::LocalFs),
                files,
            )),
            cwd,
            roots,
            cancel: CancellationToken::new(),
            shell: Arc::default(),
            images: true,
        }
    }
}

/// A permission a call needs: an action on a resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub action: String,
    pub resource: String,
}

impl Check {
    pub fn new(action: &str, resource: impl Into<String>) -> Self {
        Self {
            action: action.to_owned(),
            resource: resource.into(),
        }
    }
}

/// What a call is about, known before it runs.
#[derive(Debug, Clone)]
pub struct Invocation {
    pub title: String,
    pub kind: ToolKind,
    pub locations: Vec<PathBuf>,
    pub checks: Vec<Check>,
    /// Shown on the card while the person decides (the diff an edit would make).
    pub preview: Vec<ToolCallContent>,
    /// The tool call's `_meta` (host tools mark themselves).
    pub meta: Option<Meta>,
}

impl Invocation {
    pub fn new(title: impl Into<String>, kind: ToolKind) -> Self {
        Self {
            title: title.into(),
            kind,
            locations: Vec::new(),
            checks: Vec::new(),
            preview: Vec::new(),
            meta: None,
        }
    }
}

/// What a call produced.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    /// What the model reads.
    pub result: Vec<ToolResultPart>,
    pub is_error: bool,
    /// What the card shows; the result's text when empty.
    pub display: Vec<ToolCallContent>,
    pub raw_output: Option<Value>,
    /// A plan the call stated, for the client's plan view.
    pub plan: Option<Vec<PlanEntry>>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            result: vec![ToolResultPart::Text { text: text.into() }],
            ..Self::default()
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            result: vec![ToolResultPart::Text { text: text.into() }],
            is_error: true,
            ..Self::default()
        }
    }

    pub fn display(mut self, display: Vec<ToolCallContent>) -> Self {
        self.display = display;
        self
    }
}

/// A tool the model can call.
#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// What a call with `input` is about. An error is the model's mistake (bad arguments) and
    /// goes back to it as the call's result, without asking anyone. May read files (for a
    /// preview), off the async threads.
    async fn prepare(&self, input: &Value, context: &ToolContext) -> Result<Invocation, String>;

    /// Run the call. Cancellation is `context.cancel`.
    async fn run(&self, input: Value, context: &ToolContext) -> ToolOutput;

    /// The tool observes `context.cancel` itself and must be awaited after cancellation to finish
    /// cleanup or a file write already in progress.
    fn handles_cancellation(&self) -> bool {
        false
    }

    /// The host tool that does this tool's job better when Nori lends it (Nori's `bash` for a
    /// shell): the engine then offers that one instead.
    fn replaced_by_host(&self) -> Option<&'static str> {
        None
    }

    /// Whether the model the session runs with now is offered this tool (an image viewer needs
    /// a model that reads images). Read at every step, so a model switch changes the list.
    fn offered(&self, _model: &crate::flavor::ModelInfo) -> bool {
        true
    }
}

/// Run blocking filesystem work off the async threads.
pub(crate) async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("The tool stopped unexpectedly: {error}"))
}

/// A string argument, or the error the model reads when it is missing.
pub(crate) fn string_arg<'a>(input: &'a Value, name: &str) -> Result<&'a str, String> {
    input
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("The `{name}` argument is required and must be a string."))
}
