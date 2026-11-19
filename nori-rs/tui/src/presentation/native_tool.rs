//! Nori's own tools, shown as what they did rather than as a tool call.
//!
//! An agent built into Nori names the tool in the call's `_meta` (`nori.hostTool`), and the call
//! is recognized by that name. An agent in another process reaches the tools through the
//! `nori-client` MCP server and names them its own way (`mcp__nori-client__git_commit`,
//! `nori-client.git_commit`, ...), so there a call is recognized the way delegation calls are: by
//! the server's name and a tool name in the title. The view built here is plain data (a header
//! and detail lines with a tone each); the cell turns tones into colors.

use serde::Deserialize;
use serde::Serialize;

/// One call to a git or goal tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case")]
pub enum NativeCall {
    GitStatus,
    GitDiff { paths: Vec<String> },
    GitLog,
    GitCommit { paths: Vec<String>, message: String },
    GetGoal,
    CreateGoal { objective: String },
    UpdateGoal { status: String },
    CiStatus,
    PullRequest { title: String },
}

impl NativeCall {
    /// The call `title` names, with its arguments from `input`, when it is one.
    pub(crate) fn recognize(title: &str, input: Option<&serde_json::Value>) -> Option<Self> {
        let lowered = title.to_ascii_lowercase();
        if !lowered.contains("nori-client") && !lowered.contains("nori_client") {
            return None;
        }
        let words = lowered.replace("__", " ");
        let tool = words
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .find(|word| TOOLS.contains(word))?;
        Self::from_tool(tool, input)
    }

    /// The call to Nori's tool `tool`, with its arguments from `input`, when it is one of these.
    pub(crate) fn from_tool(tool: &str, input: Option<&serde_json::Value>) -> Option<Self> {
        let text = |key: &str| {
            input
                .and_then(|input| input.get(key))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default()
        };
        let paths = || {
            input
                .and_then(|input| input.get("paths"))
                .and_then(serde_json::Value::as_array)
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(match tool {
            "git_status" => Self::GitStatus,
            "git_diff" => Self::GitDiff { paths: paths() },
            "git_log" => Self::GitLog,
            "git_commit" => Self::GitCommit {
                paths: paths(),
                message: text("message"),
            },
            "get_goal" => Self::GetGoal,
            "create_goal" => Self::CreateGoal {
                objective: text("objective"),
            },
            "update_goal" => Self::UpdateGoal {
                status: text("status"),
            },
            "ci_status" => Self::CiStatus,
            "pull_request" => Self::PullRequest {
                title: text("title"),
            },
            _ => return None,
        })
    }
}

const TOOLS: [&str; 9] = [
    "git_status",
    "git_diff",
    "git_log",
    "git_commit",
    "get_goal",
    "create_goal",
    "update_goal",
    "ci_status",
    "pull_request",
];

/// How a piece of text should look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    /// Bold: the thing a line is about.
    Strong,
    /// Cyan: a hash, a branch, an identifier.
    Accent,
    Dim,
    Added,
    Modified,
    Deleted,
    Error,
}

/// One detail line: an optional one-letter marker, then text, then styled parts after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetailLine {
    pub marker: Option<(char, Tone)>,
    pub text: String,
    pub tone: Tone,
    pub suffix: Vec<Part>,
}

impl DetailLine {
    pub(super) fn text(text: impl Into<String>, tone: Tone) -> Self {
        Self {
            marker: None,
            text: text.into(),
            tone,
            suffix: Vec::new(),
        }
    }

    pub(super) fn file(marker: Option<(char, Tone)>, path: impl Into<String>) -> Self {
        Self {
            marker,
            text: path.into(),
            tone: Tone::Plain,
            suffix: Vec::new(),
        }
    }
}

/// A run of header text and how to style it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Part {
    pub text: String,
    pub tone: Tone,
}

pub(super) fn part(text: impl Into<String>, tone: Tone) -> Part {
    Part {
        text: text.into(),
        tone,
    }
}

/// What a native call looks like: a glyph, a header of styled parts, and detail lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NativeView {
    /// What kind of call this is, in one character.
    pub glyph: &'static str,
    pub header: Vec<Part>,
    pub detail: Vec<DetailLine>,
}

impl NativeView {
    /// The header as plain text, for tests.
    #[cfg(test)]
    pub(crate) fn header_text(&self) -> String {
        self.header.iter().map(|part| part.text.as_str()).collect()
    }

    pub(super) fn new(glyph: &'static str, header: Vec<Part>, detail: Vec<DetailLine>) -> Self {
        Self {
            glyph,
            header,
            detail,
        }
    }
}

pub(super) const GIT: &str = "⎇";
const COMMIT: &str = "◆";
const GOAL: &str = "◎";

/// Files a status lists before it says how many more there are.
pub(super) const FILES_SHOWN: usize = 8;

/// The view of `call`. `result` is the text the tool returned, once it has; `failed` is the
/// tool's own report that it did not work.
pub(crate) fn view(
    call: &NativeCall,
    active: bool,
    failed: bool,
    result: Option<&str>,
) -> NativeView {
    if failed {
        let (glyph, title) = match call {
            NativeCall::GitStatus => (GIT, "Could not read git status"),
            NativeCall::GitDiff { .. } => (GIT, "Could not read the diff"),
            NativeCall::GitLog => (GIT, "Could not read the history"),
            NativeCall::GitCommit { .. } => (COMMIT, "Commit failed"),
            NativeCall::GetGoal | NativeCall::CreateGoal { .. } | NativeCall::UpdateGoal { .. } => {
                (GOAL, "Goal tool failed")
            }
            NativeCall::CiStatus => (super::native_forge::CI, "Could not check CI"),
            NativeCall::PullRequest { .. } => {
                (super::native_forge::PULL, "Pull request not opened")
            }
        };
        // A pull request's refusal lists each rule it broke, so it keeps its lines.
        let shown = match call {
            NativeCall::PullRequest { .. } => 6,
            NativeCall::GitStatus
            | NativeCall::GitDiff { .. }
            | NativeCall::GitLog
            | NativeCall::GitCommit { .. }
            | NativeCall::GetGoal
            | NativeCall::CreateGoal { .. }
            | NativeCall::UpdateGoal { .. }
            | NativeCall::CiStatus => 1,
        };
        return NativeView::new(
            glyph,
            vec![part(title, Tone::Strong)],
            result
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .take(shown)
                .map(|line| DetailLine::text(line, Tone::Error))
                .collect(),
        );
    }
    let json = result.and_then(|text| serde_json::from_str::<serde_json::Value>(text.trim()).ok());
    match call {
        NativeCall::GitStatus => status_view(active, json.as_ref()),
        NativeCall::GitDiff { paths } => super::native_history::diff_view(active, paths, result),
        NativeCall::GitLog => super::native_history::log_view(active, json.as_ref()),
        NativeCall::GitCommit { paths, message } => {
            commit_view(active, paths, message, json.as_ref())
        }
        NativeCall::GetGoal => goal_view(
            active,
            json.as_ref()
                .and_then(|json| json.get("goal"))
                .filter(|goal| !goal.is_null()),
        ),
        NativeCall::CreateGoal { objective } => NativeView::new(
            GOAL,
            vec![part(
                if active { "Setting a goal" } else { "Goal set" },
                Tone::Strong,
            )],
            vec![DetailLine::text(objective.clone(), Tone::Dim)],
        ),
        NativeCall::CiStatus => super::native_forge::ci_view(active, json.as_ref()),
        NativeCall::PullRequest { title } => {
            super::native_forge::pull_view(active, title, json.as_ref())
        }
        NativeCall::UpdateGoal { status } => NativeView::new(
            GOAL,
            vec![
                part(
                    if active {
                        "Marking the goal "
                    } else {
                        "Goal marked "
                    },
                    Tone::Strong,
                ),
                part(status.clone(), Tone::Accent),
            ],
            Vec::new(),
        ),
    }
}

fn status_view(active: bool, json: Option<&serde_json::Value>) -> NativeView {
    let Some(json) = json else {
        return NativeView::new(
            GIT,
            vec![part(
                if active {
                    "Reading git status"
                } else {
                    "Git status"
                },
                Tone::Strong,
            )],
            Vec::new(),
        );
    };
    let branch = json.get("branch").and_then(serde_json::Value::as_str);
    let changes = json
        .get("changes")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let header = vec![
        part("Git status", Tone::Strong),
        part("  ", Tone::Plain),
        match branch {
            Some(branch) => part(branch, Tone::Accent),
            None => part("detached HEAD", Tone::Dim),
        },
    ];
    if changes.is_empty() {
        return NativeView::new(
            GIT,
            header,
            vec![DetailLine::text("✓ working tree clean", Tone::Added)],
        );
    }
    let mut detail = vec![DetailLine::text(
        format!(
            "{} {}",
            changes.len(),
            if changes.len() == 1 {
                "change"
            } else {
                "changes"
            }
        ),
        Tone::Dim,
    )];
    for change in changes.iter().take(FILES_SHOWN) {
        let path = change
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let marker = match change.get("state").and_then(serde_json::Value::as_str) {
            Some("added") => ('A', Tone::Added),
            Some("deleted") => ('D', Tone::Deleted),
            _ => ('M', Tone::Modified),
        };
        detail.push(DetailLine::file(Some(marker), path));
    }
    if changes.len() > FILES_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more", changes.len() - FILES_SHOWN),
            Tone::Dim,
        ));
    }
    NativeView::new(GIT, header, detail)
}

fn commit_view(
    active: bool,
    paths: &[String],
    message: &str,
    json: Option<&serde_json::Value>,
) -> NativeView {
    let summary = message.lines().next().unwrap_or_default().trim();
    let files = format!(
        "{} {}",
        paths.len(),
        if paths.len() == 1 { "file" } else { "files" }
    );
    if active || json.is_none() {
        let mut header = vec![part(format!("Committing {files}"), Tone::Strong)];
        if !summary.is_empty() {
            header.push(part("  ", Tone::Plain));
            header.push(part(summary, Tone::Dim));
        }
        return NativeView::new(COMMIT, header, Vec::new());
    }
    let commit = json
        .and_then(|json| json.get("commit"))
        .and_then(serde_json::Value::as_str)
        .map(|id| id.chars().take(7).collect::<String>());
    let remaining = json
        .and_then(|json| json.get("remaining_changes"))
        .and_then(serde_json::Value::as_u64);
    let mut header = vec![part("Committed", Tone::Strong), part(" ", Tone::Plain)];
    match &commit {
        Some(id) => header.push(part(id.clone(), Tone::Accent)),
        None => header.push(part(files, Tone::Plain)),
    }
    if !summary.is_empty() {
        header.push(part("  ", Tone::Plain));
        header.push(part(summary, Tone::Plain));
    }
    let mut detail: Vec<DetailLine> = paths
        .iter()
        .take(FILES_SHOWN)
        .map(|path| DetailLine::file(None, path.clone()))
        .collect();
    if paths.len() > FILES_SHOWN {
        detail.push(DetailLine::text(
            format!("… {} more", paths.len() - FILES_SHOWN),
            Tone::Dim,
        ));
    }
    match remaining {
        Some(0) => detail.push(DetailLine::text("✓ nothing left uncommitted", Tone::Added)),
        Some(left) => detail.push(DetailLine::text(
            format!("{left} still uncommitted"),
            Tone::Dim,
        )),
        None => {}
    }
    NativeView::new(COMMIT, header, detail)
}

fn goal_view(active: bool, goal: Option<&serde_json::Value>) -> NativeView {
    let title = if active { "Reading the goal" } else { "Goal" };
    let Some(goal) = goal else {
        return NativeView::new(
            GOAL,
            vec![
                part(title, Tone::Strong),
                part("  ", Tone::Plain),
                part("none set", Tone::Dim),
            ],
            Vec::new(),
        );
    };
    let objective = goal
        .get("objective")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let status = goal
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let mut header = vec![part(title, Tone::Strong)];
    if !status.is_empty() {
        header.push(part("  ", Tone::Plain));
        header.push(part(
            status,
            match status {
                "complete" => Tone::Added,
                "blocked" => Tone::Error,
                _ => Tone::Accent,
            },
        ));
    }
    NativeView::new(GOAL, header, vec![DetailLine::text(objective, Tone::Plain)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn recognizes_the_tools_however_an_agent_names_them() {
        for title in [
            "mcp__nori-client__git_status",
            "nori-client.git_status",
            "git_status (nori-client MCP Server)",
        ] {
            assert_eq!(
                NativeCall::recognize(title, None),
                Some(NativeCall::GitStatus)
            );
        }
        assert_eq!(
            NativeCall::recognize("git_status", None),
            None,
            "no server, no match"
        );
        assert_eq!(NativeCall::recognize("mcp__other__git_status", None), None);
    }

    #[test]
    fn a_commit_call_carries_its_paths_and_message() {
        let input = serde_json::json!({"paths": ["a.rs", "b.rs"], "message": "feat: x\n\nbody"});
        assert_eq!(
            NativeCall::recognize("mcp__nori-client__git_commit", Some(&input)),
            Some(NativeCall::GitCommit {
                paths: vec!["a.rs".into(), "b.rs".into()],
                message: "feat: x\n\nbody".into(),
            })
        );
    }

    #[test]
    fn a_status_lists_branch_count_and_files() {
        let result = r#"{"branch":"main","head":"abc","changes":[
            {"path":"a.rs","state":"modified"},{"path":"new.rs","state":"added"},{"path":"old.rs","state":"deleted"}]}"#;
        let view = view(&NativeCall::GitStatus, false, false, Some(result));
        assert_eq!(view.glyph, "⎇");
        assert_eq!(view.header_text(), "Git status  main");
        assert_eq!(view.header[2], part("main", Tone::Accent));
        assert_eq!(view.detail[0], DetailLine::text("3 changes", Tone::Dim));
        assert_eq!(view.detail[1].marker, Some(('M', Tone::Modified)));
        assert_eq!(view.detail[2].marker, Some(('A', Tone::Added)));
        assert_eq!(view.detail[3].marker, Some(('D', Tone::Deleted)));
        assert_eq!(view.detail[3].text, "old.rs");
    }

    #[test]
    fn a_clean_tree_and_a_long_list_both_read_well() {
        let clean = view(
            &NativeCall::GitStatus,
            false,
            false,
            Some(r#"{"branch":"main","changes":[]}"#),
        );
        assert_eq!(
            clean.detail,
            vec![DetailLine::text("✓ working tree clean", Tone::Added)]
        );

        let many: Vec<String> = (0..11)
            .map(|n| format!(r#"{{"path":"f{n}","state":"added"}}"#))
            .collect();
        let result = format!(r#"{{"branch":"main","changes":[{}]}}"#, many.join(","));
        let long = view(&NativeCall::GitStatus, false, false, Some(&result));
        assert_eq!(
            long.detail.last().map(|l| l.text.as_str()),
            Some("… 3 more")
        );
    }

    #[test]
    fn a_commit_reads_as_the_commit_it_made() {
        let call = NativeCall::GitCommit {
            paths: vec!["math.py".into()],
            message: "feat(math): add subtraction\n\nwhy".into(),
        };
        let pending = view(&call, true, false, None);
        assert_eq!(pending.glyph, "◆");
        assert_eq!(
            pending.header_text(),
            "Committing 1 file  feat(math): add subtraction"
        );

        let done = view(
            &call,
            false,
            false,
            Some(r#"{"commit":"07a5d0f9c0ffee","remaining_changes":2}"#),
        );
        assert_eq!(
            done.header_text(),
            "Committed 07a5d0f  feat(math): add subtraction"
        );
        assert_eq!(
            done.header[2],
            part("07a5d0f", Tone::Accent),
            "the hash stands out"
        );
        assert_eq!(done.detail[0].text, "math.py");
        assert_eq!(
            done.detail.last().map(|l| l.text.as_str()),
            Some("2 still uncommitted")
        );

        let clean = view(
            &call,
            false,
            false,
            Some(r#"{"commit":"07a5d0f9c0ffee","remaining_changes":0}"#),
        );
        assert_eq!(
            clean.detail.last(),
            Some(&DetailLine::text("✓ nothing left uncommitted", Tone::Added))
        );
    }

    #[test]
    fn a_failure_shows_the_tools_own_words() {
        let call = NativeCall::GitCommit {
            paths: vec!["a".into()],
            message: "x".into(),
        };
        let failed = view(
            &call,
            false,
            true,
            Some("none of those paths differ from HEAD"),
        );
        assert_eq!(failed.header_text(), "Commit failed");
        assert_eq!(
            failed.detail,
            vec![DetailLine::text(
                "none of those paths differ from HEAD",
                Tone::Error
            )]
        );
    }

    #[test]
    fn goal_calls_show_the_objective_and_color_the_status() {
        let got = view(
            &NativeCall::GetGoal,
            false,
            false,
            Some(r#"{"goal":{"objective":"Ship it","status":"active"}}"#),
        );
        assert_eq!(got.header_text(), "Goal  active");
        assert_eq!(got.header[2].tone, Tone::Accent);
        assert_eq!(got.detail, vec![DetailLine::text("Ship it", Tone::Plain)]);

        let done = view(
            &NativeCall::GetGoal,
            false,
            false,
            Some(r#"{"goal":{"objective":"x","status":"complete"}}"#),
        );
        assert_eq!(done.header[2].tone, Tone::Added);
        assert_eq!(
            view(&NativeCall::GetGoal, false, false, Some(r#"{"goal":null}"#)).header_text(),
            "Goal  none set"
        );
    }
}
