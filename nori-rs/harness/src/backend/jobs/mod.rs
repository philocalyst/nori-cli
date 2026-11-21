//! Background jobs of one actor: commands that run on this computer or a machine of the roster,
//! outlive the turn and the session, and wake the agent instead of being polled.
//!
//! Execution belongs to the supervisor in `nori-machines`; this is the registry above it:
//!
//! - [`Jobs`]: the actor's jobs, each remembering how far the agent has read its output (the read
//!   cursor), so `job_read` returns exactly what is new and never repeats a line.
//! - [`spec`]: what a job is asked for ([`StartJob`]) and what is said back.
//! - [`journal`], [`restore`]: one `job` command per change in the conversation journal; a resume
//!   adopts every job that was running, and says so when one is gone.
//! - [`wake`]: each job with a wake is followed by a monitor of the `job` source, so everything a
//!   monitor does with lines (batching, `fail_on`, delivery through the turn ledger) a job gets
//!   without a second pipeline.
//! - [`view`]: listing, reading and stopping, as the tools show them.
//!
//! Invariants: a job's process is never owned by Nori's session (closing only stops following);
//! a line is read once per cursor; the journal says a job ended only when its state was final.

mod journal;
mod moves;
mod request;
mod restore;
mod spec;
mod view;
mod wake;
mod watches;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::OnceLock;
use std::sync::PoisonError;

use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobHandle;
use nori_machines::jobs::JobId;
use nori_machines::jobs::JobLine;
use nori_machines::jobs::JobRecord;
use nori_machines::jobs::JobState;
use nori_machines::jobs::MachineSupervisor;
use nori_machines::jobs::Supervisor;
use tokio::sync::broadcast;
use tokio::sync::watch;

use self::journal::Record;
pub(crate) use self::request::JobReadRequest;
pub(crate) use self::request::JobStartRequest;
pub(crate) use self::request::JobStopRequest;
pub(crate) use self::spec::JobOrigin;
pub(crate) use self::spec::JobSummary;
pub(crate) use self::spec::JobWake;
pub(crate) use self::spec::ReadSpec;
pub(crate) use self::spec::StartJob;
use self::spec::place_name;
use self::spec::state_text;
pub(crate) use self::watches::DeclaredWatch;
use super::monitor::Monitors;
use crate::transcript::TranscriptRecorder;

/// The most jobs one actor may have running.
pub(crate) const MAX_RUNNING: i64 = 16;
/// Ended jobs kept in the list.
const KEEP_ENDED: usize = 16;

/// The cell holding the session's conversation journal.
pub(crate) type Journal = Arc<tokio::sync::RwLock<Option<Arc<TranscriptRecorder>>>>;

#[derive(Clone)]
pub(crate) struct Jobs {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Jobs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Jobs").finish_non_exhaustive()
    }
}

struct Shared {
    state: Mutex<State>,
    /// Installed explicitly; else the one registered for `home`; else the machines' own, created
    /// on first use so a session that never runs a job never reads the roster.
    supervisor: Mutex<Option<Arc<dyn Supervisor>>>,
    /// The session's `NORI_HOME`: how a world finds the supervisor it scripted for this session.
    home: PathBuf,
    journal: Mutex<Option<Journal>>,
    /// Wakes are monitors; a change in the jobs also changes what the monitor loop publishes.
    monitors: Monitors,
    /// What the session's monitor modules watch on job modules, armed when such a job starts.
    watches: Mutex<Vec<DeclaredWatch>>,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
    closed: bool,
}

struct Entry {
    record: JobRecord,
    /// The label, unique among the running jobs.
    label: String,
    origin: JobOrigin,
    wake: JobWake,
    /// What follows the job; `None` for a job that ended in an earlier run, or was not found.
    handle: Option<JobHandle>,
    state: JobState,
    /// Everything before this the agent has read.
    cursor: Cursor,
    /// Journals the job's end and keeps `state` current.
    watcher: Option<tokio::task::AbortHandle>,
}

impl Entry {
    fn running(&self) -> bool {
        !self.state.is_final()
    }

    fn summary(&self) -> JobSummary {
        JobSummary {
            job: self.record.id.to_string(),
            label: self.label.clone(),
            place: place_name(&self.record.place),
            pid: self.record.pid,
            state: state_text(&self.state),
            note: None,
        }
    }
}

/// A job as a monitor's source follows it.
pub(crate) enum Followed {
    Live {
        handle: JobHandle,
        lines: broadcast::Receiver<JobLine>,
        state: watch::Receiver<JobState>,
    },
    /// Nothing follows it any more; this is how it stands.
    Over(JobState),
}

impl Followed {
    /// Subscribe now: lines that arrive from this moment are kept for the follower.
    pub(crate) fn live(handle: &JobHandle) -> Self {
        Self::Live {
            lines: handle.lines(),
            state: handle.state(),
            handle: handle.clone(),
        }
    }
}

impl Jobs {
    /// `monitors` follow the jobs that have a wake, and publish their count; `home` is the
    /// session's `NORI_HOME`.
    pub(crate) fn new(monitors: Monitors, home: PathBuf) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                supervisor: Mutex::new(None),
                home,
                journal: Mutex::new(None),
                monitors,
                watches: Mutex::new(Vec::new()),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.shared
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Journal into the session's conversation from now on.
    pub(crate) fn attach_journal(&self, journal: Journal) {
        *self
            .shared
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(journal);
    }

    /// Use `supervisor` instead of the machines' own: a test or a world scripts the jobs.
    #[allow(dead_code)] // The API of tests and worlds; the backend itself never calls it.
    pub(crate) fn install_supervisor(&self, supervisor: Arc<dyn Supervisor>) {
        *self
            .shared
            .supervisor
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(supervisor);
    }

    fn supervisor(&self) -> Arc<dyn Supervisor> {
        Arc::clone(
            self.shared
                .supervisor
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get_or_insert_with(|| {
                    supervisor_for_home(&self.shared.home)
                        .unwrap_or_else(|| Arc::new(MachineSupervisor::of_this_process()))
                }),
        )
    }

    async fn write(&self, record: &Record) {
        let journal = self
            .shared
            .journal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(journal) = journal else { return };
        let Some(recorder) = journal.read().await.clone() else {
            return;
        };
        match serde_json::to_string(record) {
            Ok(payload) => {
                if let Err(error) = recorder.record_command(journal::COMMAND, payload).await {
                    tracing::warn!(%error, "failed to journal a job change");
                }
            }
            Err(error) => tracing::warn!(%error, "failed to serialize a job change"),
        }
    }

    /// Start `request.command` in the background and follow it. The wake the request asks for is
    /// armed before this returns; the job runs on whatever happens to the turn that started it.
    pub(crate) async fn start(&self, request: StartJob) -> Result<JobSummary, String> {
        let wanted = request.label.as_deref().map(str::trim).unwrap_or_default();
        if wanted.chars().count() > 40 {
            return Err("`label` must be a short name, at most 40 characters".to_owned());
        }
        let label = {
            let state = self.lock();
            if state.closed {
                return Err("this session is ending; no job can be started".to_owned());
            }
            let running: Vec<String> = state
                .entries
                .iter()
                .filter(|entry| entry.running())
                .map(|entry| format!("{} ({})", entry.record.id, entry.label))
                .collect();
            if i64::try_from(running.len()).unwrap_or(i64::MAX) >= MAX_RUNNING {
                return Err(format!(
                    "{MAX_RUNNING} jobs are already running: {}. `job_stop` one first.",
                    running.join(", ")
                ));
            }
            if let Some(clash) = state
                .entries
                .iter()
                .find(|entry| entry.running() && entry.label == wanted)
            {
                return Err(format!(
                    "a job labelled `{wanted}` is already running ({}). `job_stop` it, or use \
                     another label.",
                    clash.record.id
                ));
            }
            if wanted.is_empty() {
                let first = request.command.split_whitespace().next().unwrap_or("job");
                let base = first.rsplit('/').next().unwrap_or(first);
                unique_label(&state, &base.chars().take(24).collect::<String>())
            } else {
                wanted.to_owned()
            }
        };
        let origin = request.origin.clone();
        let wake = request.wake.clone();
        let spec = request.into_spec(&*nori_machines::global_ready().await, label)?;
        let handle = self
            .supervisor()
            .start(spec)
            .await
            .map_err(|error| error.to_string())?;
        self.adopt_handle(handle, origin, wake).await
    }

    /// Follow a job somebody else started (the host `bash` tool, a module): list it, journal it,
    /// and arm the wake `wake` asks for.
    pub(crate) async fn adopt_handle(
        &self,
        handle: JobHandle,
        origin: JobOrigin,
        wake: JobWake,
    ) -> Result<JobSummary, String> {
        // Subscribe before anything awaits: output that arrives while this journals is not lost.
        let followed = Followed::live(&handle);
        let mut record = handle.record().clone();
        let (mut summary, id) = {
            let mut state = self.lock();
            if state.closed {
                return Err("this session is ending; no job can be followed".to_owned());
            }
            record.label = unique_label(&state, &record.label);
            let entry = Entry {
                label: record.label.clone(),
                record: record.clone(),
                origin: origin.clone(),
                wake: wake.clone(),
                handle: Some(handle.clone()),
                state: handle.state().borrow().clone(),
                cursor: Cursor::default(),
                watcher: None,
            };
            let summary = entry.summary();
            state.entries.push(entry);
            trim_ended(&mut state);
            (summary, record.id.clone())
        };
        self.write(&Record::Started {
            record: record.clone(),
            origin: origin.clone(),
            wake: wake.clone(),
        })
        .await;
        // After `Started`, so a job that ends at once is never journaled ended before it began.
        self.watch(&id, &handle);
        let mut problems: Vec<String> = self
            .arm_wake(&id, &record.label, &wake, followed)
            .await
            .err()
            .into_iter()
            .collect();
        problems.extend(self.arm_declared(&id, &record.label, &origin).await);
        summary.note = (!problems.is_empty()).then(|| problems.join("; "));
        self.shared.monitors.notify();
        Ok(summary)
    }

    /// Keep an entry's state current and journal its end.
    fn watch(&self, id: &JobId, handle: &JobHandle) {
        let jobs = self.clone();
        let followed = id.clone();
        let mut state = handle.state();
        let task = tokio::spawn(async move {
            loop {
                let current = state.borrow_and_update().clone();
                let ended = current.is_final();
                jobs.note_state(&followed, current).await;
                if ended || state.changed().await.is_err() {
                    break;
                }
            }
        })
        .abort_handle();
        if let Some(entry) = self
            .lock()
            .entries
            .iter_mut()
            .find(|entry| entry.record.id == *id)
        {
            entry.watcher = Some(task);
        }
    }

    async fn note_state(&self, id: &JobId, current: JobState) {
        let changed = {
            let mut state = self.lock();
            match state
                .entries
                .iter_mut()
                .find(|entry| entry.record.id == *id)
            {
                Some(entry) if entry.state != current => {
                    entry.state = current.clone();
                    true
                }
                Some(_) | None => false,
            }
        };
        if !changed {
            return;
        }
        if current.is_final() {
            self.write(&Record::Ended {
                id: id.clone(),
                state: current,
            })
            .await;
        }
        self.shared.monitors.notify();
    }

    /// What a `job` monitor follows: the job `job` names (an id).
    pub(crate) fn follow(&self, job: &str) -> Result<Followed, String> {
        let state = self.lock();
        let entry = state
            .entries
            .iter()
            .find(|entry| entry.record.id.as_str() == job)
            .ok_or_else(|| format!("`{job}` is not a job of yours"))?;
        Ok(match &entry.handle {
            Some(handle) => Followed::live(handle),
            None => Followed::Over(entry.state.clone()),
        })
    }

    /// The session ends: stop following. The jobs keep running, and a resume adopts them.
    pub(crate) fn close(&self) {
        let mut state = self.lock();
        state.closed = true;
        for entry in &mut state.entries {
            if let Some(watcher) = entry.watcher.take() {
                watcher.abort();
            }
            entry.handle = None;
        }
        drop(state);
        self.shared.monitors.notify();
    }
}

/// Supervisors registered for a `NORI_HOME`: each world session has a home of its own.
fn homes() -> &'static Mutex<HashMap<PathBuf, Arc<dyn Supervisor>>> {
    static HOMES: OnceLock<Mutex<HashMap<PathBuf, Arc<dyn Supervisor>>>> = OnceLock::new();
    HOMES.get_or_init(Mutex::default)
}

fn supervisor_for_home(home: &Path) -> Option<Arc<dyn Supervisor>> {
    homes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(home)
        .cloned()
}

/// Give the sessions whose `NORI_HOME` is `home` a scripted supervisor (a world's) instead of the
/// machines' own. Takes effect for a session's first job; `Jobs::install_supervisor` on a session
/// still wins.
pub fn install_supervisor_for_home(home: PathBuf, supervisor: Arc<dyn Supervisor>) {
    homes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(home, supervisor);
}

/// Forget the supervisor registered for `home`.
pub fn remove_supervisor_for_home(home: &Path) {
    homes()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(home);
}

/// `wanted`, or `wanted-2`, `wanted-3`: a label no running job has.
fn unique_label(state: &State, wanted: &str) -> String {
    let base = if wanted.is_empty() { "job" } else { wanted };
    let taken = |label: &str| {
        state
            .entries
            .iter()
            .any(|entry| entry.running() && entry.label == label)
    };
    let mut label = base.to_owned();
    let mut number = 1;
    while taken(&label) {
        number += 1;
        label = format!("{base}-{number}");
    }
    label
}

/// Keep the list of ended jobs short: the oldest go first.
fn trim_ended(state: &mut State) {
    let ended = state
        .entries
        .iter()
        .filter(|entry| !entry.running())
        .count();
    let mut surplus = ended.saturating_sub(KEEP_ENDED);
    state.entries.retain(|entry| {
        if !entry.running() && surplus > 0 {
            surplus -= 1;
            false
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests;
