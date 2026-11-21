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
    /// What an event does. None means: wake the agent with it (what every monitor armed before
    /// actions existed does).
    #[serde(default)]
    pub(crate) actions: Vec<Action>,
    #[serde(default)]
    pub(crate) origin: Origin,
}

/// Who armed a monitor.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub(crate) enum Origin {
    /// The agent, with `monitor_start` (or Nori on its behalf, as the CI watch).
    #[default]
    Agent,
    /// A `nori:Monitor` module, armed for every session it applies to.
    Module { path: PathBuf },
}

impl Spec {
    /// A prompt from a `turn_ended` monitor ends as another user turn, which would fire the same
    /// action forever. Other session events and sources do not have that feedback path.
    pub(crate) fn validate_actions(&self) -> Result<(), String> {
        if self
            .actions
            .iter()
            .any(|action| matches!(action, Action::Prompt { .. }))
            && matches!(
                &self.source,
                Source::Session { events, .. } if events.contains(&SessionTrigger::TurnEnded)
            )
        {
            return Err(
                "a `turn_ended` session monitor cannot use a `prompt` action; its prompt would trigger it again"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Whether events of this monitor reach the agent in a wake.
    pub(crate) fn wakes(&self) -> bool {
        self.actions.is_empty()
            || self
                .actions
                .iter()
                .any(|action| matches!(action, Action::Wake { .. }))
    }

    /// What leads this monitor's events in a wake, if its `wake` action says.
    pub(crate) fn wake_message(&self) -> Option<&str> {
        self.actions.iter().find_map(|action| match action {
            Action::Wake { message } => message.as_deref(),
            Action::Prompt { .. }
            | Action::Notify { .. }
            | Action::RunJob { .. }
            | Action::StopJob { .. }
            | Action::RunSkill { .. } => None,
        })
    }
}

#[cfg(test)]
mod action_tests {
    use super::*;

    fn spec(source: Source, actions: Vec<Action>) -> Spec {
        Spec {
            label: "test".to_owned(),
            source,
            rules: Rules::default(),
            wake: WakeMode::default(),
            limits: Limits {
                timeout_seconds: 60,
                max_events: 10,
                batch_ms: 100,
                digest_seconds: 30,
            },
            actions,
            origin: Origin::Agent,
        }
    }

    #[test]
    fn a_turn_ended_monitor_cannot_prompt_itself_again() {
        let actions = vec![Action::Prompt {
            text: "continue".to_owned(),
        }];
        let turn_ended = spec(
            Source::Session {
                events: vec![SessionTrigger::TurnEnded],
                idle_seconds: None,
            },
            actions.clone(),
        );
        assert!(turn_ended.validate_actions().is_err());

        let failed_turn = spec(
            Source::Session {
                events: vec![SessionTrigger::TurnFailed],
                idle_seconds: None,
            },
            actions,
        );
        assert!(failed_turn.validate_actions().is_ok());
    }
}

/// What a monitor does with an event. Templates may use `{{event}}` (the event's lines),
/// `{{label}}` and `{{monitor}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Action {
    /// Hand the event to the agent in a hidden turn, when it is idle. `message` leads it.
    Wake {
        #[serde(default)]
        message: Option<String>,
    },
    /// Queue a visible prompt, as if the person had typed it.
    Prompt { text: String },
    /// Tell the person: an OS notification and one line in the transcript. The agent is not woken.
    Notify {
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        body: Option<String>,
    },
    /// Start the job module (`jobs/NAME.pkl`) named `job`, with its own wake. A job of that name
    /// already running is left alone.
    RunJob { job: String },
    /// Stop the running job labelled `job` (a module job is labelled with its name): TERM, then
    /// KILL after 5 seconds.
    StopJob { job: String },
    /// Run the skill (`skills/NAME.pkl`) named `skill` with `arguments` (templates fill in), in
    /// the background. Its result wakes the agent when it ends, whatever the other actions are.
    RunSkill {
        skill: String,
        #[serde(default)]
        arguments: std::collections::BTreeMap<String, String>,
    },
}

/// The moments of a session a `session` monitor reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(crate) enum SessionTrigger {
    /// A turn ended (not one a monitor's wake started).
    TurnEnded,
    /// A request failed and the person was told why.
    TurnFailed,
    /// The session moved to another agent, model or account.
    RouteChanged,
    /// A subagent completed, failed, was cancelled or closed.
    SubagentEnded,
    /// The agent asked for permission.
    PermissionRequested,
    /// A usage window of the account (5 hours, a week, ...) passed 80%, 95% or 100% used, once
    /// per level per window until it resets.
    QuotaLow,
}

impl SessionTrigger {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::TurnEnded => "turn_ended",
            Self::TurnFailed => "turn_failed",
            Self::RouteChanged => "route_changed",
            Self::SubagentEnded => "subagent_ended",
            Self::PermissionRequested => "permission_requested",
            Self::QuotaLow => "quota_low",
        }
    }
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
    /// A clock: a period, an instant or a cron schedule. Each firing is an event.
    Timer { schedule: super::source::Schedule },
    /// The session itself: turns ending or failing, route changes, subagents ending, permission
    /// requests, and staying idle for `idle_seconds`.
    Session {
        events: Vec<SessionTrigger>,
        #[serde(default)]
        idle_seconds: Option<i64>,
    },
    /// A background job (see `backend::jobs`): its output lines, when it is ready, and how it
    /// ends. The monitor's `rules.filter` is the job's `on_output` regex.
    Job {
        /// The job's id.
        job: String,
        label: String,
        /// Output lines are events (the ones `rules.filter` keeps).
        lines: bool,
        /// A regex: the first output line that matches is one event, "job ready".
        ready: Option<String>,
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
            Self::Timer { .. } => "timer",
            Self::Session { .. } => "session",
            Self::Job { .. } => "job",
        }
    }

    /// Whether running out of time is just the end of the watch, not a failure: a schedule has
    /// nothing it waits to see, so its end says nothing went wrong.
    pub(crate) fn expires_quietly(&self) -> bool {
        match self {
            Self::Timer { .. } | Self::Session { .. } => true,
            Self::Command { .. }
            | Self::File { .. }
            | Self::Watch { .. }
            | Self::Actor { .. }
            | Self::Ci { .. }
            | Self::HttpPoll { .. }
            | Self::Job { .. } => false,
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
            Self::Timer { schedule } => schedule.describe(),
            Self::Session {
                events,
                idle_seconds,
            } => {
                let mut what: Vec<String> = events
                    .iter()
                    .map(|event| event.label().to_owned())
                    .collect();
                if let Some(seconds) = idle_seconds {
                    what.push(format!("idle {seconds}s"));
                }
                format!("session: {}", what.join(", "))
            }
            Self::Job { label, job, .. } => format!("job {label} ({job})"),
        }
    }

    /// How long a monitor of this source runs when not told: long enough for what it waits for.
    pub(crate) fn default_timeout_seconds(&self) -> i64 {
        match self {
            Self::Command { .. } | Self::Actor { .. } | Self::HttpPoll { .. } => 1800,
            Self::File { .. } | Self::Watch { .. } => 3600,
            Self::Ci { .. } => 2700,
            Self::Timer { .. } | Self::Session { .. } | Self::Job { .. } => MAX_TIMEOUT_SECONDS,
        }
    }

    /// How long events of this source are gathered into one: a burst of file changes is one
    /// event, a stream of output lines nearly is.
    pub(crate) fn default_batch_ms(&self) -> i64 {
        match self {
            Self::Watch { .. } => 600,
            Self::Command { .. } | Self::File { .. } | Self::Job { .. } => 300,
            Self::Actor { .. }
            | Self::Ci { .. }
            | Self::HttpPoll { .. }
            | Self::Timer { .. }
            | Self::Session { .. } => 100,
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
