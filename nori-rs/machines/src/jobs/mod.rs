//! Jobs: commands that run in the background, on this computer or another machine, and outlive
//! the turn, the connection and Nori itself.
//!
//! A job is a process group of its own on its machine, with its output appended to files there
//! (see `script`), so nothing is lost while nobody watches and a restarted (or moved) Nori adopts
//! it again from its [`JobRecord`]. A [`JobHandle`] follows a job: its lines as they come
//! ([`JobHandle::lines`]), its state ([`JobHandle::state`]), and exact reads of its output from
//! any [`Cursor`] ([`JobHandle::read`]), the same before and after a restart.
//!
//! Policy (what an agent may start, when it is woken, how jobs show in the TUI) is not here: the
//! runtime builds that on [`Supervisor`], which tests and worlds can replace.

mod script;
mod supervisor;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio::sync::watch;

pub use supervisor::MachineSupervisor;

use crate::ExecError;
use crate::MachineName;
use crate::StartDir;
use crate::Stream;
use crate::UnknownMachine;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a job runs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Place {
    /// The computer this Nori runs on.
    Local,
    Machine(MachineName),
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local => f.write_str("this computer"),
            Self::Machine(name) => write!(f, "{name}"),
        }
    }
}

/// A job's name: unique on its machine.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(String);

impl JobId {
    /// A fresh id: time-ordered, so a listing of `~/.nori/jobs` reads oldest first.
    pub fn fresh() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        Self(format!("j{nanos:x}{:x}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for JobId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// A script, run by bash when the machine has it, else `sh`.
    pub command: String,
    pub place: Place,
    pub start: StartDir,
    /// Set for the job only; never written to disk on any machine.
    pub env: BTreeMap<String, String>,
    /// What people call it.
    pub label: String,
}

/// Everything needed to find a job again: what the runtime journals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: JobId,
    pub place: Place,
    pub label: String,
    pub command: String,
    /// The runner's pid, which is also the job's process group.
    pub pid: i64,
    /// The machine's boot when the job started: a different boot means the job is gone.
    pub boot: String,
    /// Seconds since the Unix epoch.
    pub started_at: i64,
}

/// The boot a job inside a Nori process records: it lives as long as that process, and no
/// later Nori can adopt it.
const IN_PROCESS_BOOT: &str = "nori-process:";

impl JobRecord {
    /// The record of a job that runs inside this Nori (a `bash` command that outlived its
    /// timeout): listed and read like any job, lost when this process ends.
    pub fn in_process(label: String, command: String) -> Self {
        Self {
            id: JobId::fresh(),
            place: Place::Local,
            label,
            command,
            pid: i64::from(std::process::id()),
            boot: format!("{IN_PROCESS_BOOT}{}", std::process::id()),
            started_at: crate::facts::now_seconds(),
        }
    }

    /// Whether the job ran inside a Nori process, and so cannot be adopted by another.
    pub fn is_in_process(&self) -> bool {
        self.boot.starts_with(IN_PROCESS_BOOT)
    }
}

/// A position in a job's output: how many bytes of each stream come before it. Exact across
/// reconnects, restarts and moves, because the bytes are in files on the job's machine.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub stdout: i64,
    pub stderr: i64,
}

/// One line of a job's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobLine {
    pub stream: Stream,
    pub text: String,
    /// Where the output stands just after this line.
    pub after: Cursor,
    /// Counts the lines this handle has seen, for live consumers.
    pub seq: i64,
}

/// How a job is doing, as far as Nori can tell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JobState {
    Running {
        pid: i64,
    },
    Exited {
        code: i64,
    },
    Killed {
        signal: String,
    },
    /// Its machine does not answer now; the job may well be running. Nori retries.
    Unreachable {
        reason: String,
    },
    /// The job is gone for good: its machine restarted, or its files are gone.
    Lost {
        reason: String,
    },
}

impl JobState {
    pub fn is_final(&self) -> bool {
        match self {
            Self::Exited { .. } | Self::Killed { .. } | Self::Lost { .. } => true,
            Self::Running { .. } | Self::Unreachable { .. } => false,
        }
    }

    /// `exit 0`, `signal TERM`, as the job's `exit` file says it.
    pub(crate) fn from_exit_line(line: &str) -> Self {
        let line = line.trim();
        if let Some(code) = line.strip_prefix("exit ")
            && let Ok(code) = code.trim().parse()
        {
            return Self::Exited { code };
        }
        if let Some(signal) = line.strip_prefix("signal ") {
            return Self::Killed {
                signal: signal.trim().to_owned(),
            };
        }
        Self::Lost {
            reason: format!("it ended with an unreadable status ({line})"),
        }
    }
}

/// Where a read starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFrom {
    /// Exactly from here, in whole lines.
    Cursor(Cursor),
    /// The last bytes of each stream, starting at a line boundary.
    Tail { bytes: i64 },
    /// All of it, a last line without a newline included: for a job that has ended.
    Everything,
}

#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error(transparent)]
    UnknownMachine(#[from] UnknownMachine),
    #[error(transparent)]
    Machine(#[from] ExecError),
    #[error("the job could not start on {place}: {reason}")]
    Launch { place: Place, reason: String },
    #[error("the job is gone: {reason}")]
    Lost { reason: String },
}

/// Starts jobs and finds them again. One per process; tests and worlds supply their own.
pub trait Supervisor: Send + Sync {
    fn start(&self, spec: JobSpec) -> BoxFuture<'_, Result<JobHandle, JobError>>;

    /// Follow a job started earlier, by this Nori or another (a restart, a moved session).
    fn adopt(&self, record: JobRecord) -> BoxFuture<'_, Result<JobHandle, JobError>>;
}

/// Output read from a job: whole lines of each stream, and where the next read starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobOutput {
    pub lines: Vec<JobLine>,
    pub next: Cursor,
}

/// Something a [`JobHandle`] asks of its job's machine.
pub trait JobControl: Send + Sync {
    fn read(&self, from: ReadFrom, max_bytes: i64) -> BoxFuture<'_, Result<JobOutput, JobError>>;
    fn signal(&self, signal: Signal) -> BoxFuture<'_, Result<(), JobError>>;
    fn stop(&self, grace: Duration) -> BoxFuture<'_, Result<(), JobError>>;

    /// The directory the command ended in, once it has ended and said.
    fn final_cwd(&self) -> Option<String> {
        None
    }

    /// Remove what the job left on its machine; for a job nobody reads again.
    fn forget(&self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async { Ok(()) })
    }
}

/// Signals an agent may send a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Signal {
    Int,
    Term,
    Hup,
    Kill,
    Usr1,
    Usr2,
}

impl Signal {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Int => "INT",
            Self::Term => "TERM",
            Self::Hup => "HUP",
            Self::Kill => "KILL",
            Self::Usr1 => "USR1",
            Self::Usr2 => "USR2",
        }
    }
}

/// A followed job. Dropping every handle stops following; the job keeps running.
pub struct JobHandle {
    record: JobRecord,
    lines: broadcast::Sender<JobLine>,
    state: watch::Receiver<JobState>,
    control: std::sync::Arc<dyn JobControl>,
    /// Ends the follower with the last handle.
    _follower: std::sync::Arc<FollowerGuard>,
}

impl Clone for JobHandle {
    fn clone(&self) -> Self {
        Self {
            record: self.record.clone(),
            lines: self.lines.clone(),
            state: self.state.clone(),
            control: std::sync::Arc::clone(&self.control),
            _follower: std::sync::Arc::clone(&self._follower),
        }
    }
}

pub(crate) struct FollowerGuard(pub(crate) tokio::task::AbortHandle);

impl Drop for FollowerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl JobHandle {
    /// Assemble a handle; supervisors other than [`MachineSupervisor`] (a world's scripted
    /// one) build theirs with this.
    pub fn new(
        record: JobRecord,
        lines: broadcast::Sender<JobLine>,
        state: watch::Receiver<JobState>,
        control: std::sync::Arc<dyn JobControl>,
        follower: tokio::task::AbortHandle,
    ) -> Self {
        Self {
            record,
            lines,
            state,
            control,
            _follower: std::sync::Arc::new(FollowerGuard(follower)),
        }
    }

    pub fn record(&self) -> &JobRecord {
        &self.record
    }

    /// Lines from now on. A slow receiver misses lines (`Lagged`); [`Self::read`] has them all.
    pub fn lines(&self) -> broadcast::Receiver<JobLine> {
        self.lines.subscribe()
    }

    pub fn state(&self) -> watch::Receiver<JobState> {
        self.state.clone()
    }

    /// Output from `from`, at most `max_bytes` of each stream, in whole lines.
    pub async fn read(&self, from: ReadFrom, max_bytes: i64) -> Result<JobOutput, JobError> {
        self.control.read(from, max_bytes).await
    }

    pub async fn signal(&self, signal: Signal) -> Result<(), JobError> {
        self.control.signal(signal).await
    }

    /// TERM, then KILL after `grace`.
    pub async fn stop(&self, grace: Duration) -> Result<(), JobError> {
        self.control.stop(grace).await
    }

    /// The directory the command ended in, once it has ended.
    pub fn final_cwd(&self) -> Option<String> {
        self.control.final_cwd()
    }

    /// Remove what an ended job left on its machine.
    pub async fn forget(&self) -> Result<(), JobError> {
        self.control.forget().await
    }

    /// Wait until the job ends, or `limit` passes; the state either way.
    pub async fn wait(&self, limit: Duration) -> JobState {
        let mut state = self.state.clone();
        let ended = tokio::time::timeout(limit, state.wait_for(JobState::is_final)).await;
        match ended {
            Ok(Ok(state)) => state.clone(),
            Ok(Err(_)) | Err(_) => self.state.borrow().clone(),
        }
    }
}
