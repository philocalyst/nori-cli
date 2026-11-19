//! What a monitor is, as it was armed: its source, its rules and its limits. A spec is what the
//! journal stores, so a monitor armed in one run can be restarted in the next.

use std::path::PathBuf;

use regex::Regex;
use serde::Deserialize;
use serde::Serialize;

/// The most monitors one actor may have running.
pub(crate) const MAX_RUNNING: i64 = 8;
/// The most events one monitor may be allowed to emit.
pub(crate) const MAX_EVENTS_CEILING: i64 = 500;
/// The longest a monitor may run.
pub(crate) const MAX_TIMEOUT_SECONDS: i64 = 8 * 3600;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Spec {
    /// What its events are called: unique among the actor's running monitors.
    pub(crate) label: String,
    pub(crate) source: Source,
    #[serde(default)]
    pub(crate) rules: Rules,
    #[serde(default)]
    pub(crate) wake: WakeMode,
    pub(crate) limits: Limits,
}

/// What a monitor watches. Each variant carries exactly what its source needs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Source {
    /// A shell command run in the background; each stdout line is an event, and its exit is the
    /// last one.
    Command {
        command: String,
        cwd: PathBuf,
        /// Stderr lines are events too. Otherwise they are kept, and the end of them comes with
        /// a failed exit.
        stderr: bool,
    },
    /// Lines appended to a file.
    File { path: PathBuf, from_start: bool },
    /// Entries created, modified and deleted under paths.
    Watch {
        paths: Vec<PathBuf>,
        kinds: Vec<ChangeKind>,
    },
    /// What another actor does: its state, its results, and what it says to this actor.
    Actor { actor: String, track: Vec<Track> },
    /// The checks on a commit, until they decide.
    Ci {
        /// The commit; the branch's pushed commit when absent.
        sha: Option<String>,
        branch: Option<String>,
        /// Set when a workflow armed the watch: what its CI hooks tell the agent to do.
        #[serde(default)]
        follow_up: Option<CiFollowUp>,
    },
    /// A URL read on an interval; an event only when what was extracted from it changes.
    HttpPoll {
        url: String,
        interval_seconds: i64,
        extract: Extract,
    },
}

impl Source {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::File { .. } => "file",
            Self::Watch { .. } => "watch",
            Self::Actor { .. } => "actor",
            Self::Ci { .. } => "ci",
            Self::HttpPoll { .. } => "http_poll",
        }
    }

    /// One line saying what is watched, for lists.
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Command { command, .. } => command.clone(),
            Self::File { path, .. } => path.display().to_string(),
            Self::Watch { paths, .. } => paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", "),
            Self::Actor { actor, .. } => actor.clone(),
            Self::Ci { sha, branch, .. } => match (sha, branch) {
                (Some(sha), _) => sha.clone(),
                (None, Some(branch)) => format!("branch {branch}"),
                (None, None) => "the branch's pushed commit".to_owned(),
            },
            Self::HttpPoll { url, .. } => url.clone(),
        }
    }

    /// How long a monitor of this source runs when not told: long enough for what it waits for.
    pub(crate) fn default_timeout_seconds(&self) -> i64 {
        match self {
            Self::Command { .. } | Self::Actor { .. } | Self::HttpPoll { .. } => 1800,
            Self::File { .. } | Self::Watch { .. } => 3600,
            Self::Ci { .. } => 2700,
        }
    }

    /// How long events of this source are gathered into one: a burst of file changes is one
    /// event, a stream of output lines nearly is.
    pub(crate) fn default_batch_ms(&self) -> i64 {
        match self {
            Self::Watch { .. } => 600,
            Self::Command { .. } | Self::File { .. } => 300,
            Self::Actor { .. } | Self::Ci { .. } | Self::HttpPoll { .. } => 100,
        }
    }
}

/// What a workflow's CI hooks say to do with a verdict, resolved when the watch was armed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CiFollowUp {
    pub(crate) workflow: String,
    pub(crate) on_failed: Option<String>,
    pub(crate) on_passed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(crate) enum ChangeKind {
    Created,
    Modified,
    Deleted,
}

impl ChangeKind {
    pub(crate) const ALL: [Self; 3] = [Self::Created, Self::Modified, Self::Deleted];

    pub(crate) fn verb(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(crate) enum Track {
    State,
    Results,
    Replies,
}

impl Track {
    pub(crate) const ALL: [Self; 3] = [Self::State, Self::Results, Self::Replies];
}

/// What an `http_poll` monitor reads from a response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", content = "expression", rename_all = "snake_case")]
pub(crate) enum Extract {
    /// The HTTP status: `HTTP 200`.
    Status,
    JsonPath(String),
    /// The first capture group of the first match, or the whole match.
    Regex(String),
}

/// What turns raw lines into events.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Rules {
    /// Only lines that match are events.
    #[serde(default)]
    pub(crate) filter: Option<String>,
    /// Lines that match are always events, marked as failures, whatever `filter` says.
    #[serde(default)]
    pub(crate) fail_on: Vec<String>,
    /// The monitor ends after emitting the first line that matches.
    #[serde(default)]
    pub(crate) until: Option<String>,
}

/// [`Rules`] with their patterns compiled.
pub(crate) struct CompiledRules {
    pub(crate) filter: Option<Regex>,
    pub(crate) fail_on: Vec<Regex>,
    pub(crate) until: Option<Regex>,
}

impl Rules {
    /// Compile the patterns; the error names the one that is not a regular expression.
    pub(crate) fn compile(&self) -> Result<CompiledRules, String> {
        let one = |name: &str, pattern: &str| {
            Regex::new(pattern).map_err(|error| {
                format!("`{name}` is not a valid regular expression ({pattern:?}): {error}")
            })
        };
        Ok(CompiledRules {
            filter: self
                .filter
                .as_deref()
                .map(|pattern| one("filter", pattern))
                .transpose()?,
            fail_on: self
                .fail_on
                .iter()
                .map(|pattern| one("fail_on", pattern))
                .collect::<Result<_, _>>()?,
            until: self
                .until
                .as_deref()
                .map(|pattern| one("until", pattern))
                .transpose()?,
        })
    }
}

/// When events wake an agent that is idle.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema,
)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(crate) enum WakeMode {
    /// As soon as they arrive.
    #[default]
    Immediate,
    /// Not by themselves: with the next turn boundary.
    Boundary,
    /// Gathered for `digest_seconds` into one wake.
    Digest,
}

impl WakeMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Immediate => "immediate",
            Self::Boundary => "boundary",
            Self::Digest => "digest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Limits {
    /// The monitor ends, reporting that it expired, after this long.
    pub(crate) timeout_seconds: i64,
    /// The monitor stops, reporting a flood, after this many events.
    pub(crate) max_events: i64,
    /// Lines that arrive within this long of each other are one event.
    pub(crate) batch_ms: i64,
    /// How long a digest gathers events.
    pub(crate) digest_seconds: i64,
}
