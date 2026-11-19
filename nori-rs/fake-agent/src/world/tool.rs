//! Tool calls and permission requests, as a script describes them.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use super::step::Step;
use super::value::Span;
use super::value::Text;

/// What a tool does, as ACP names it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKindDef {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatusDef {
    #[default]
    Pending,
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    #[default]
    Completed,
    Failed,
}

/// A file a tool touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub path: String,
    #[serde(default)]
    pub line: Option<i64>,
}

/// What a tool call shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentDef {
    /// Plain text.
    Text { text: Text },
    /// A file change with the real old and new text, so edit journals and diffs work. An absent
    /// `old_text` creates the file.
    Diff {
        path: String,
        #[serde(default)]
        old_text: Option<String>,
        new_text: String,
    },
    /// A terminal the client holds.
    Terminal { terminal_id: String },
}

/// How a tool's updates are cut out of order, to see what the client does with them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Disorder {
    /// The ordinary lifecycle: announce, progress, one terminal update.
    #[default]
    None,
    /// The terminal update arrives before the call is announced.
    EndBeforeStart,
    /// The call is announced twice.
    DuplicateStart,
    /// An update arrives after the terminal one, regressing the status to in progress.
    UpdateAfterEnd,
    /// The terminal update is sent twice, the second with the opposite outcome.
    ContradictoryEnd,
    /// The call is announced and never ends.
    NeverEnds,
}

/// One streamed update of a running tool.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Progress {
    /// Pause before this update.
    pub after: Span,
    pub status: Option<ToolStatusDef>,
    pub title: Option<String>,
    /// Replaces the content shown so far when set.
    pub content: Option<Vec<ContentDef>>,
    /// Appended to the standard output shown so far (`raw_output.stdout`, and as text content).
    pub stdout: Option<Text>,
    pub output: Option<Value>,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            after: Span::ZERO,
            status: None,
            title: None,
            content: None,
            stdout: None,
            output: None,
        }
    }
}

/// A whole tool call: announced, streamed, ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSpec {
    /// The call id; generated (`tool-N`) when absent.
    pub id: Option<String>,
    pub title: String,
    #[serde(rename = "tool_kind")]
    pub kind: ToolKindDef,
    pub locations: Vec<Location>,
    /// `raw_input`. For an edit with a single diff and no input, `file_path`, `old_string` and
    /// `new_string` are derived from the diff, the way Nori's own agents report edits.
    pub input: Option<Value>,
    /// The status of the announcement.
    pub status: ToolStatusDef,
    /// Content shown on the announcement.
    pub content: Vec<ContentDef>,
    pub progress: Vec<Progress>,
    /// Content of the terminal update. Defaults to the announced content.
    pub result: Option<Vec<ContentDef>>,
    /// `raw_output`.
    pub output: Option<Value>,
    /// Standard output: `raw_output.stdout`, and text content of the terminal update.
    pub stdout: Option<Text>,
    pub stderr: Option<Text>,
    pub exit_code: Option<i64>,
    /// Time between the announcement and the terminal update, split around the progress updates.
    pub duration: Span,
    pub outcome: ToolOutcome,
    /// `_meta` on the announcement and the terminal update.
    pub meta: Option<Value>,
    /// Name of a host tool this call is, so Nori renders it as one (`_meta.nori.hostTool`).
    pub host_tool: Option<String>,
    pub disorder: Disorder,
}

impl Default for ToolSpec {
    fn default() -> Self {
        Self {
            id: None,
            title: String::new(),
            kind: ToolKindDef::Other,
            locations: Vec::new(),
            input: None,
            status: ToolStatusDef::Pending,
            content: Vec::new(),
            progress: Vec::new(),
            result: None,
            output: None,
            stdout: None,
            stderr: None,
            exit_code: None,
            duration: Span::ZERO,
            outcome: ToolOutcome::Completed,
            meta: None,
            host_tool: None,
            disorder: Disorder::None,
        }
    }
}

/// Announce a call and leave it there; later steps update it, in any order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolStart {
    pub id: String,
    pub title: String,
    #[serde(rename = "tool_kind")]
    pub kind: ToolKindDef,
    pub locations: Vec<Location>,
    pub input: Option<Value>,
    pub status: ToolStatusDef,
    pub content: Vec<ContentDef>,
    pub meta: Option<Value>,
}

impl Default for ToolStart {
    fn default() -> Self {
        Self {
            id: String::new(),
            title: String::new(),
            kind: ToolKindDef::Other,
            locations: Vec::new(),
            input: None,
            status: ToolStatusDef::Pending,
            content: Vec::new(),
            meta: None,
        }
    }
}

/// A `tool_call_update` for any call id, whether or not it was announced or has ended.
#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolUpdateStep {
    pub id: String,
    pub status: Option<ToolStatusDef>,
    pub title: Option<String>,
    pub content: Option<Vec<ContentDef>>,
    pub locations: Option<Vec<Location>>,
    pub output: Option<Value>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OptionKindDef {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

impl OptionKindDef {
    pub fn allows(self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

/// One button of a permission prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionDef {
    pub id: String,
    pub name: String,
    pub kind: OptionKindDef,
}

/// What the user decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Answer {
    Allow,
    Deny,
    /// The client withdrew the question: the turn was cancelled or the connection ended.
    Cancelled,
    /// Nobody answered within `timeout`.
    Timeout,
}

/// Ask for permission to run a tool, then run it if allowed. The branches say what the agent
/// does after each answer, so a script can retry, give up, or press on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionStep {
    pub tool: ToolSpec,
    /// Options offered. Defaults to Allow, Always allow, Deny.
    pub options: Vec<OptionDef>,
    /// What the script was written to be answered. A different answer is recorded as a
    /// divergence the driver reports; the script still branches on what was actually answered.
    pub expect: Option<Answer>,
    /// How long to wait for an answer; unbounded when absent.
    pub timeout: Option<Span>,
    /// Run the tool's lifecycle when allowed (otherwise the permission is all there is).
    pub run_when_allowed: bool,
    pub on_allow: Vec<Step>,
    pub on_deny: Vec<Step>,
    pub on_cancelled: Vec<Step>,
    pub on_timeout: Vec<Step>,
}

impl Default for PermissionStep {
    fn default() -> Self {
        Self {
            tool: ToolSpec::default(),
            options: Vec::new(),
            expect: None,
            timeout: None,
            run_when_allowed: true,
            on_allow: Vec::new(),
            on_deny: Vec::new(),
            on_cancelled: Vec::new(),
            on_timeout: Vec::new(),
        }
    }
}
