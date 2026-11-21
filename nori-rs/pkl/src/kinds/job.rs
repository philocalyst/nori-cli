//! The `nori:Job` kind: a named background process.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;

use super::check_name;
use super::check_not_empty;
use super::check_regex;
use super::finish;

/// When the job counts as started.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    /// A regular expression matched against each output line.
    pub pattern: String,
    /// How long to wait for a matching line.
    #[serde(with = "super::duration")]
    pub timeout: Duration,
}

/// When a restart happens after the job ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestartOn {
    /// Never restart.
    Never,
    /// Restart after a non-zero exit.
    Failure,
    /// Restart after any exit that was not a stop.
    Always,
}

/// What to do when the job ends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restart {
    /// When to restart.
    pub on: RestartOn,
    /// How many restarts in a row before giving up.
    pub max: i64,
    /// The wait before the first restart.
    #[serde(with = "super::duration")]
    pub backoff: Duration,
}

/// What wakes the agent that started the job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobWake {
    /// Wake when the job exits.
    pub on_exit: bool,
    /// Wake when the job becomes ready.
    pub on_ready: bool,
    /// Wake when an output line matches this regular expression.
    #[serde(default)]
    pub on_output: Option<String>,
    /// Wake when the job is lost.
    pub on_lost: bool,
}

/// A job module: what to run, where, and how to supervise it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobDef {
    /// The job's name (`[a-z0-9][a-z0-9_-]*`).
    pub name: String,
    /// One line saying what the job is.
    pub description: String,
    /// The shell command to run.
    pub command: String,
    /// `local`, or a machine from the roster.
    pub machine: String,
    /// The working directory; the session's when unset.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Extra environment variables.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// How to tell the job is ready; ready as soon as it runs when unset.
    #[serde(default)]
    pub ready: Option<Ready>,
    /// What to do when the job ends.
    pub restart: Restart,
    /// What wakes the agent.
    pub wake: JobWake,
    /// A shell command whose exit status 0 means the job did what it should.
    #[serde(default)]
    pub check: Option<String>,
    /// Each output stream rotates at this many bytes.
    pub max_log_bytes: i64,
    /// Stop the job after it has run this long.
    #[serde(default, with = "super::optional_duration")]
    pub timeout: Option<Duration>,
    /// How long a stop waits after TERM before it sends KILL.
    #[serde(with = "super::duration")]
    pub stop_grace: Duration,
}

impl JobDef {
    /// Checks what the types cannot: the name, the command, the regular expressions and the
    /// numbers. Every message names the field.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        check_name("name", &self.name, &mut problems);
        check_not_empty("command", &self.command, &mut problems);
        check_not_empty("machine", &self.machine, &mut problems);
        if let Some(ready) = &self.ready {
            check_regex("ready.pattern", &ready.pattern, &mut problems);
        }
        if let Some(pattern) = &self.wake.on_output {
            check_regex("wake.on_output", pattern, &mut problems);
        }
        if let Some(check) = &self.check {
            check_not_empty("check", check, &mut problems);
        }
        if self.restart.max < 0 {
            problems.push(format!(
                "restart.max: must not be negative, got {}",
                self.restart.max
            ));
        }
        if self.max_log_bytes <= 0 {
            problems.push(format!(
                "max_log_bytes: must be greater than zero, got {}",
                self.max_log_bytes
            ));
        }
        if self.timeout == Some(Duration::ZERO) {
            problems.push("timeout: must be greater than zero".to_owned());
        }
        finish(problems)
    }
}
