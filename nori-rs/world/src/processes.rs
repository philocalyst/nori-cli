//! The world's machine: jobs a session starts (`job_start`, a background `bash`) run the world's
//! scripted processes (`processes` in `nori:World`) instead of anything real.
//!
//! [`WorldSupervisor`] is a [`Supervisor`] like the one that runs real jobs, so everything above it
//! (the registry, the journal, the wakes, the tools) is the harness's own code under test. A
//! process prints its lines at the pace of the world's clock, keeps them in memory with byte
//! cursors as a real job's log files do, and ends with its exit code, or runs until it is stopped.
//!
//! The pace: the harness's own timers (the monitors that follow a job batch what it prints for a
//! few hundred milliseconds) run on real time whatever the world's clock is, so a virtual clock,
//! which jumps over every gap the moment the agents are idle, would play a whole process between
//! two of those batches and make every line and the exit one wake. A process therefore waits in
//! real time, scaled by the world's speed when it has one (`x30` plays it thirty times faster); a
//! virtual world plays it at its scripted speed. A line with no `after` is printed without waiting,
//! so a flood is as fast as the machine.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nori_fake_agent::ClockMode;
use nori_fake_agent::ProcessDef;
use nori_fake_agent::StreamDef;
use nori_machines::Stream;
use nori_machines::jobs::BoxFuture;
use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobControl;
use nori_machines::jobs::JobError;
use nori_machines::jobs::JobHandle;
use nori_machines::jobs::JobId;
use nori_machines::jobs::JobLine;
use nori_machines::jobs::JobOutput;
use nori_machines::jobs::JobRecord;
use nori_machines::jobs::JobSpec;
use nori_machines::jobs::JobState;
use nori_machines::jobs::ReadFrom;
use nori_machines::jobs::Signal;
use nori_machines::jobs::Supervisor;
use tokio::sync::broadcast;
use tokio::sync::watch;

/// Runs a world's scripted processes as jobs.
pub struct WorldSupervisor {
    processes: Vec<ProcessDef>,
    next_pid: AtomicI64,
    /// How much faster than its script a process plays: the world's speed, or 1.
    speed: f64,
}

impl WorldSupervisor {
    pub fn new(processes: Vec<ProcessDef>, clock: ClockMode) -> Self {
        let speed = match clock {
            ClockMode::Virtual => 1.0,
            ClockMode::Real(speed) => speed,
        };
        Self {
            processes,
            next_pid: AtomicI64::new(4000),
            speed,
        }
    }
}

/// One printed line, where it starts and where the output stands after it.
struct Printed {
    stream: Stream,
    text: String,
    start: Cursor,
    after: Cursor,
}

#[derive(Default)]
struct Log {
    lines: Vec<Printed>,
    end: Cursor,
}

struct Control {
    log: Arc<Mutex<Log>>,
    state: watch::Sender<JobState>,
    task: tokio::task::AbortHandle,
}

impl Control {
    fn kill(&self, signal: &str) {
        self.task.abort();
        self.state.send_if_modified(|state| {
            if state.is_final() {
                return false;
            }
            *state = JobState::Killed {
                signal: signal.to_owned(),
            };
            true
        });
    }
}

impl JobControl for Control {
    fn read(&self, from: ReadFrom, max_bytes: i64) -> BoxFuture<'_, Result<JobOutput, JobError>> {
        let log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        let start = match from {
            ReadFrom::Cursor(cursor) => cursor,
            ReadFrom::Everything => Cursor::default(),
            // The last `bytes` of each stream, from a line's start.
            ReadFrom::Tail { bytes } => Cursor {
                stdout: (log.end.stdout - bytes).max(0),
                stderr: (log.end.stderr - bytes).max(0),
            },
        };
        let mut budget = Cursor {
            stdout: max_bytes,
            stderr: max_bytes,
        };
        let mut full = (false, false);
        let mut next = start;
        let mut lines = Vec::new();
        for printed in &log.lines {
            let (begins, length, left, closed) = match printed.stream {
                Stream::Stdout => (
                    printed.start.stdout >= start.stdout,
                    printed.after.stdout - printed.start.stdout,
                    &mut budget.stdout,
                    &mut full.0,
                ),
                Stream::Stderr => (
                    printed.start.stderr >= start.stderr,
                    printed.after.stderr - printed.start.stderr,
                    &mut budget.stderr,
                    &mut full.1,
                ),
            };
            // A stream's read stops at the first line that does not fit: a later, shorter line
            // must not be returned past it, or the cursor would skip the one left behind.
            if !begins || *closed {
                continue;
            }
            if length > *left {
                *closed = true;
                continue;
            }
            *left -= length;
            match printed.stream {
                Stream::Stdout => next.stdout = printed.after.stdout,
                Stream::Stderr => next.stderr = printed.after.stderr,
            }
            lines.push(JobLine {
                stream: printed.stream,
                text: printed.text.clone(),
                after: printed.after,
                seq: 0,
            });
        }
        Box::pin(std::future::ready(Ok(JobOutput { lines, next })))
    }

    fn signal(&self, signal: Signal) -> BoxFuture<'_, Result<(), JobError>> {
        // A scripted process is not told anything it could ignore: every signal ends it.
        self.kill(&format!("{signal:?}").to_uppercase());
        Box::pin(std::future::ready(Ok(())))
    }

    fn stop(&self, _grace: Duration) -> BoxFuture<'_, Result<(), JobError>> {
        self.kill("TERM");
        Box::pin(std::future::ready(Ok(())))
    }
}

impl Supervisor for WorldSupervisor {
    fn start(&self, spec: JobSpec) -> BoxFuture<'_, Result<JobHandle, JobError>> {
        let Some(process) = self
            .processes
            .iter()
            .find(|process| process.runs(&spec.command))
            .cloned()
        else {
            return Box::pin(std::future::ready(Err(JobError::Launch {
                place: spec.place,
                reason: format!(
                    "this world runs no process for {:?}: add one to its `processes`",
                    spec.command
                ),
            })));
        };
        let pid = self.next_pid.fetch_add(1, Ordering::Relaxed);
        let record = JobRecord {
            id: JobId::fresh(),
            place: spec.place,
            label: spec.label,
            command: spec.command,
            pid,
            boot: "world".to_owned(),
            started_at: chrono::Utc::now().timestamp(),
        };
        let (lines, _) = broadcast::channel(256);
        let (state, state_rx) = watch::channel(JobState::Running { pid });
        let log = Arc::new(Mutex::new(Log::default()));
        let task = tokio::spawn(play(
            process,
            self.speed,
            Arc::clone(&log),
            lines.clone(),
            state.clone(),
        ));
        let control = Control {
            log,
            state,
            task: task.abort_handle(),
        };
        // The follower is the playing task itself: the last handle dropped stops it, as a real
        // follower stops following (a scripted process has nothing to outlive Nori in).
        let follower = task.abort_handle();
        Box::pin(std::future::ready(Ok(JobHandle::new(
            record,
            lines,
            state_rx,
            Arc::new(control),
            follower,
        ))))
    }

    fn adopt(&self, record: JobRecord) -> BoxFuture<'_, Result<JobHandle, JobError>> {
        Box::pin(std::future::ready(Err(JobError::Lost {
            reason: format!(
                "{} was a world's scripted process, which does not outlive its world",
                record.label
            ),
        })))
    }
}

async fn play(
    process: ProcessDef,
    speed: f64,
    log: Arc<Mutex<Log>>,
    lines: broadcast::Sender<JobLine>,
    state: watch::Sender<JobState>,
) {
    for (seq, line) in (1_i64..).zip(&process.output) {
        if line.after.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(line.after.0.div_f64(speed)).await;
        }
        let stream = match line.stream {
            StreamDef::Stdout => Stream::Stdout,
            StreamDef::Stderr => Stream::Stderr,
        };
        let length = i64::try_from(line.text.len()).unwrap_or(i64::MAX) + 1;
        let printed = {
            let mut log = log.lock().unwrap_or_else(PoisonError::into_inner);
            let start = log.end;
            match stream {
                Stream::Stdout => log.end.stdout += length,
                Stream::Stderr => log.end.stderr += length,
            }
            let after = log.end;
            log.lines.push(Printed {
                stream,
                text: line.text.clone(),
                start,
                after,
            });
            JobLine {
                stream,
                text: line.text.clone(),
                after,
                seq,
            }
        };
        let _ = lines.send(printed);
    }
    let Some(code) = process.exit else {
        return;
    };
    if !process.linger.is_zero() {
        tokio::time::sleep(process.linger.0.div_f64(speed)).await;
    }
    state.send_replace(JobState::Exited { code });
}
