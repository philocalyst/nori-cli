//! What a job is asked for, and what is said back about it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use nori_machines::RemoteDir;
use nori_machines::Roster;
use nori_machines::StartDir;
use nori_machines::Target;
use nori_machines::jobs::JobSpec;
use nori_machines::jobs::JobState;
use nori_machines::jobs::Place;
use serde::Deserialize;
use serde::Serialize;

use crate::backend::monitor::source::resolve_path;

/// Who started a job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub(crate) enum JobOrigin {
    /// The agent, with `job_start`.
    Agent,
    /// The host `bash` tool, with `background: true`.
    Bash { call_id: Option<String> },
    /// A `nori:Job` module, by name.
    Module { name: String },
}

/// What wakes the agent that started a job. A job that asks for nothing is still listed and can be
/// read; it just never speaks first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct JobWake {
    /// Wake the agent when the job ends. With no other wake asked for, `false` arms nothing. When
    /// `on_output` or `ready` arms a watch anyway, the job's end is reported with it.
    pub(crate) on_exit: bool,
    /// A regex: output lines that match wake the agent.
    pub(crate) on_output: Option<String>,
    /// A regex: the first output line that matches wakes the agent once ("it is ready").
    pub(crate) ready: Option<String>,
}

impl Default for JobWake {
    fn default() -> Self {
        Self {
            on_exit: true,
            on_output: None,
            ready: None,
        }
    }
}

impl JobWake {
    /// Whether any wake is asked for.
    pub(crate) fn is_armed(&self) -> bool {
        self.on_exit || self.on_output.is_some() || self.ready.is_some()
    }
}

/// A request to run a command in the background.
pub(crate) struct StartJob {
    pub(crate) command: String,
    /// What people call it; derived from the command when absent.
    pub(crate) label: Option<String>,
    /// `local` (or nothing) for this computer, else a machine of the roster.
    pub(crate) machine: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) origin: JobOrigin,
    pub(crate) wake: JobWake,
    /// Where relative paths start on this computer, and the project's place for a machine's default.
    pub(crate) local_cwd: PathBuf,
}

impl StartJob {
    /// The supervisor's spec: where it runs and where it starts. A directory the agent named must
    /// exist; one it did not name falls back, so a missing default never blocks the job.
    pub(super) fn into_spec(self, roster: &Roster, label: String) -> Result<JobSpec, String> {
        let command = self.command.trim().to_owned();
        if command.is_empty() {
            return Err("a job needs a `command`".to_owned());
        }
        let target = roster
            .resolve(self.machine.as_deref())
            .map_err(|error| error.to_string())?;
        let (place, start) = match target {
            Target::Local => {
                let dir = self.cwd.as_deref().map_or_else(
                    || self.local_cwd.clone(),
                    |dir| resolve_path(&self.local_cwd, dir),
                );
                if !dir.is_dir() {
                    return Err(format!(
                        "the working directory {} does not exist",
                        dir.display()
                    ));
                }
                (
                    Place::Local,
                    StartDir::Required(RemoteDir::Path(dir.display().to_string())),
                )
            }
            Target::Remote(machine) => (
                Place::Machine(machine.name.clone()),
                match self.cwd.as_deref() {
                    Some(dir) => StartDir::Required(RemoteDir::parse(dir)),
                    None => StartDir::FirstOf(roster.default_dirs(machine, &self.local_cwd)),
                },
            ),
        };
        Ok(JobSpec {
            command,
            place,
            start,
            env: self.env,
            label,
        })
    }
}

/// What starting, stopping or adopting a job says about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct JobSummary {
    pub(crate) job: String,
    pub(crate) label: String,
    pub(crate) place: String,
    pub(crate) pid: i64,
    pub(crate) state: String,
    /// Something the caller should know: the wake could not be armed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) note: Option<String>,
}

/// Where a read starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadSpec {
    /// Exactly what the agent has not read yet.
    New,
    /// The last `bytes` of each stream.
    Tail { bytes: i64 },
    /// The beginning.
    Start,
    #[allow(dead_code)] // Not a tool argument: for callers that keep their own place.
    Cursor(nori_machines::jobs::Cursor),
}

/// A place's name as the agent says it: `local`, or the machine.
pub(super) fn place_name(place: &Place) -> String {
    match place {
        Place::Local => "local".to_owned(),
        Place::Machine(name) => name.as_str().to_owned(),
    }
}

/// A job's state in a few words.
pub(super) fn state_text(state: &JobState) -> String {
    match state {
        JobState::Running { .. } => "running".to_owned(),
        JobState::Exited { code } => format!("exited {code}"),
        JobState::Killed { signal } => format!("killed by {signal}"),
        JobState::Unreachable { reason } => format!("unreachable: {reason}"),
        JobState::Lost { reason } => format!("lost: {reason}"),
    }
}
