//! The `nori:Monitor` kind: triggers, an optional condition, and actions.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::DateTime;
use chrono::NaiveTime;
use serde::Deserialize;
use serde::Serialize;

use super::check_name;
use super::check_not_empty;
use super::check_regex;
use super::finish;

/// An event of the session itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionEvent {
    /// A turn ended.
    TurnEnded,
    /// A turn failed.
    TurnFailed,
    /// The route (agent and model) changed.
    RouteChanged,
    /// A subagent ended.
    SubagentEnded,
    /// The agent asked for a permission.
    PermissionRequested,
    /// A quota is running low.
    QuotaLow,
}

/// What happened to a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobEvent {
    /// The job became ready.
    Ready,
    /// The job exited.
    Exited,
    /// The job failed.
    Failed,
    /// The job was lost.
    Lost,
    /// The job printed a line.
    Output,
}

/// A change to a watched file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatchKind {
    /// A file was created.
    Created,
    /// A file was modified.
    Modified,
    /// A file was deleted.
    Deleted,
}

/// What to follow of a peer actor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorTrack {
    /// Its state changes.
    State,
    /// Its results.
    Results,
    /// Its replies.
    Replies,
}

/// How a wake is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WakeMode {
    /// At the next safe point.
    Immediate,
    /// Gathered for the digest window and delivered together.
    Digest,
    /// Only when the turn ends.
    Boundary,
}

/// Something that makes a monitor fire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Trigger {
    /// A period.
    Every {
        /// The time between firings.
        #[serde(with = "super::duration")]
        interval: Duration,
    },
    /// An instant.
    At {
        /// An RFC 3339 timestamp or a local `HH:MM`.
        time: String,
    },
    /// A cron schedule.
    Cron {
        /// Five fields, or six with seconds first.
        expression: String,
    },
    /// The session has been idle that long.
    Idle {
        /// The idle time.
        #[serde(with = "super::duration")]
        after: Duration,
    },
    /// Events of the session.
    Session {
        /// Which events.
        events: Vec<SessionEvent>,
    },
    /// What happens to a job.
    Job {
        /// The job's name.
        job: String,
        /// Which changes.
        on: Vec<JobEvent>,
    },
    /// Lines of a command run for the monitor alone.
    Output {
        /// The shell command.
        command: String,
        /// The working directory.
        #[serde(default)]
        cwd: Option<String>,
        /// Also watch the error stream.
        stderr: bool,
    },
    /// Lines appended to a file.
    File {
        /// The file.
        path: String,
        /// Read the lines already there too.
        from_start: bool,
    },
    /// Files created, modified or deleted.
    Watch {
        /// Paths or directories.
        paths: Vec<String>,
        /// Which changes; all when empty.
        kinds: Vec<WatchKind>,
    },
    /// A value at a URL that changed.
    Http {
        /// The URL.
        url: String,
        /// The time between checks.
        #[serde(with = "super::duration")]
        interval: Duration,
        /// A dotted path into a JSON response.
        #[serde(default)]
        json_path: Option<String>,
        /// A regular expression applied to the response.
        #[serde(default)]
        regex: Option<String>,
    },
    /// CI checks that decided.
    Ci {
        /// The branch; the current one when unset.
        #[serde(default)]
        branch: Option<String>,
    },
    /// A peer actor's activity.
    Actor {
        /// The actor's alias.
        actor: String,
        /// What to follow; everything when empty.
        track: Vec<ActorTrack>,
    },
}

/// Something done when a monitor fires.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// A hidden prompt to the agent.
    Wake {
        /// The prompt; `{{event}}` is what fired.
        #[serde(default)]
        message: Option<String>,
    },
    /// A visible prompt, as if the person had typed it.
    Prompt {
        /// The prompt's text.
        text: String,
    },
    /// An OS notification and one transcript line.
    Notify {
        /// The title.
        #[serde(default)]
        title: Option<String>,
        /// The text.
        #[serde(default)]
        body: Option<String>,
    },
    /// Start a job.
    RunJob {
        /// The job's name.
        job: String,
    },
    /// Stop a job.
    StopJob {
        /// The job's name.
        job: String,
    },
    /// Run a skill.
    RunSkill {
        /// The skill's name.
        skill: String,
        /// Values for the skill's parameters.
        arguments: BTreeMap<String, String>,
    },
}

/// A monitor module.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorDef {
    /// The monitor's name (`[a-z0-9][a-z0-9_-]*`).
    pub name: String,
    /// One line saying what the monitor is for.
    pub description: String,
    /// What makes it fire; at least one.
    pub on: Vec<Trigger>,
    /// A regular expression an event's text must match.
    #[serde(default)]
    pub filter: Option<String>,
    /// Regular expressions that mark an event as a failure.
    #[serde(default)]
    pub fail_on: Vec<String>,
    /// A regular expression that ends the monitor.
    #[serde(default)]
    pub until: Option<String>,
    /// What to do when it fires; empty means wake the agent.
    #[serde(default)]
    pub actions: Vec<Action>,
    /// How a wake is delivered.
    pub wake: WakeMode,
    /// How long a digest gathers events.
    #[serde(with = "super::duration")]
    pub digest: Duration,
    /// Gather bursts closer together than this into one firing.
    #[serde(default, with = "super::optional_duration")]
    pub debounce: Option<Duration>,
    /// End after this many firings.
    #[serde(default)]
    pub max_fires: Option<i64>,
    /// End after this long.
    #[serde(default, with = "super::optional_duration")]
    pub expires: Option<Duration>,
}

impl MonitorDef {
    /// Checks what the types cannot: the name, that there is a trigger, the regular expressions,
    /// the schedules and the bounds. Every message names the field.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        check_name("name", &self.name, &mut problems);
        if self.on.is_empty() {
            problems.push("on: a monitor needs at least one trigger".to_owned());
        }
        for (index, trigger) in self.on.iter().enumerate() {
            let at = format!("on[{index}]");
            match trigger {
                Trigger::Every { interval } => {
                    if interval.is_zero() {
                        problems.push(format!("{at}.interval: must be greater than zero"));
                    }
                }
                Trigger::At { time } => {
                    if DateTime::parse_from_rfc3339(time).is_err()
                        && NaiveTime::parse_from_str(time, "%H:%M").is_err()
                    {
                        problems.push(format!(
                            "{at}.time: {time:?} is neither an RFC 3339 timestamp nor a local HH:MM"
                        ));
                    }
                }
                Trigger::Cron { expression } => {
                    let fields = expression.split_whitespace().count();
                    if fields != 5 && fields != 6 {
                        problems.push(format!(
                            "{at}.expression: {expression:?} has {fields} fields, expected 5 (or 6 with seconds first)"
                        ));
                    }
                }
                Trigger::Idle { after } => {
                    if after.is_zero() {
                        problems.push(format!("{at}.after: must be greater than zero"));
                    }
                }
                Trigger::Session { events } => {
                    if events.is_empty() {
                        problems.push(format!("{at}.events: name at least one event"));
                    }
                }
                Trigger::Job { job, on } => {
                    check_name(&format!("{at}.job"), job, &mut problems);
                    if on.is_empty() {
                        problems.push(format!("{at}.on: name at least one job event"));
                    }
                }
                Trigger::Output { command, .. } => {
                    check_not_empty(&format!("{at}.command"), command, &mut problems);
                }
                Trigger::File { path, .. } => {
                    check_not_empty(&format!("{at}.path"), path, &mut problems);
                }
                Trigger::Watch { paths, .. } => {
                    if paths.is_empty() {
                        problems.push(format!("{at}.paths: name at least one path"));
                    }
                }
                Trigger::Http {
                    url,
                    interval,
                    regex,
                    ..
                } => {
                    check_not_empty(&format!("{at}.url"), url, &mut problems);
                    if interval.is_zero() {
                        problems.push(format!("{at}.interval: must be greater than zero"));
                    }
                    if let Some(pattern) = regex {
                        check_regex(&format!("{at}.regex"), pattern, &mut problems);
                    }
                }
                Trigger::Ci { .. } => {}
                Trigger::Actor { actor, .. } => {
                    check_not_empty(&format!("{at}.actor"), actor, &mut problems);
                }
            }
        }
        if let Some(pattern) = &self.filter {
            check_regex("filter", pattern, &mut problems);
        }
        for (index, pattern) in self.fail_on.iter().enumerate() {
            check_regex(&format!("fail_on[{index}]"), pattern, &mut problems);
        }
        if let Some(pattern) = &self.until {
            check_regex("until", pattern, &mut problems);
        }
        for (index, action) in self.actions.iter().enumerate() {
            let at = format!("actions[{index}]");
            match action {
                Action::Wake { .. } | Action::Notify { .. } => {}
                Action::Prompt { text } => {
                    check_not_empty(&format!("{at}.text"), text, &mut problems);
                }
                Action::RunJob { job } | Action::StopJob { job } => {
                    check_name(&format!("{at}.job"), job, &mut problems);
                }
                Action::RunSkill { skill, .. } => {
                    check_name(&format!("{at}.skill"), skill, &mut problems);
                }
            }
        }
        if self.digest.is_zero() {
            problems.push("digest: must be greater than zero".to_owned());
        }
        if let Some(max_fires) = self.max_fires
            && max_fires <= 0
        {
            problems.push(format!(
                "max_fires: must be greater than zero, got {max_fires}"
            ));
        }
        if self.expires == Some(Duration::ZERO) {
            problems.push("expires: must be greater than zero".to_owned());
        }
        finish(problems)
    }
}
