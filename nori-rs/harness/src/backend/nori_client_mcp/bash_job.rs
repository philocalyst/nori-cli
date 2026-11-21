//! A local `bash` command that outlives its timeout keeps running, as a job.
//!
//! The command runs in the caller's persistent shell. When it does not finish within the call's
//! timeout, the shell is handed over to the job with the command still running in it, the caller
//! gets a fresh shell in the same directory, and the job is followed like any other: listed,
//! readable from any cursor, stoppable, and its end wakes the agent. It lives as long as this Nori
//! does (an in-process job cannot be adopted after a restart; see `JobRecord::in_process`).

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use nori_machines::Stream;
use nori_machines::jobs::BoxFuture;
use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobControl;
use nori_machines::jobs::JobError;
use nori_machines::jobs::JobHandle;
use nori_machines::jobs::JobLine;
use nori_machines::jobs::JobOutput;
use nori_machines::jobs::JobRecord;
use nori_machines::jobs::JobState;
use nori_machines::jobs::ReadFrom;
use nori_machines::jobs::Signal;
use tokio::sync::broadcast;
use tokio::sync::oneshot;
use tokio::sync::watch;

/// The most output an in-process job keeps of each stream; older bytes are dropped (and the
/// cursor still counts them).
const KEEP_BYTES: usize = 8 * 1024 * 1024;
/// A command that became a job runs at most this long.
const JOB_LIMIT_MS: i64 = 8 * 3600 * 1000;

/// A command running in a persistent shell, which can become a job.
pub(super) struct Running {
    pub(super) done: oneshot::Receiver<Result<nori_bash::BashOutput, nori_bash::BashError>>,
    shared: Arc<Shared>,
    lines: broadcast::Sender<JobLine>,
    state: watch::Receiver<JobState>,
    task: tokio::task::AbortHandle,
}

struct Shared {
    log: Mutex<Log>,
    cancel: nori_bash::CancellationToken,
}

#[derive(Default)]
struct Log {
    out: Kept,
    err: Kept,
    seq: i64,
}

/// The end of a stream, and how many bytes came before what is kept.
#[derive(Default)]
struct Kept {
    bytes: Vec<u8>,
    dropped: i64,
}

impl Kept {
    fn push(&mut self, line: &str) -> i64 {
        self.bytes.extend_from_slice(line.as_bytes());
        self.bytes.push(b'\n');
        if self.bytes.len() > KEEP_BYTES {
            let excess = self.bytes.len() - KEEP_BYTES;
            self.bytes.drain(..excess);
            self.dropped += excess as i64;
        }
        self.end()
    }

    fn end(&self) -> i64 {
        self.dropped + self.bytes.len() as i64
    }

    /// Whole lines from offset `from`, at most `max` bytes.
    fn lines_from(&self, from: i64, max: i64) -> (Vec<String>, i64) {
        let start = usize::try_from((from - self.dropped).max(0)).unwrap_or_default();
        let start = start.min(self.bytes.len());
        let available = &self.bytes[start..];
        let limit = usize::try_from(max.max(0))
            .unwrap_or_default()
            .min(available.len());
        let window = &available[..limit];
        let whole = window
            .iter()
            .rposition(|&byte| byte == b'\n')
            .map_or(0, |at| at + 1);
        let lines = window[..whole]
            .split(|&byte| byte == b'\n')
            .filter(|_| whole > 0)
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect::<Vec<_>>();
        let mut lines = lines;
        // `split` yields an empty piece after the final newline.
        if lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        (lines, self.dropped + (start + whole) as i64)
    }
}

/// Start `command` in `session`, which this holds until the command ends.
pub(super) fn start(
    session: Arc<tokio::sync::Mutex<nori_bash::BashSession>>,
    command: String,
) -> Running {
    let shared = Arc::new(Shared {
        log: Mutex::new(Log::default()),
        cancel: nori_bash::CancellationToken::new(),
    });
    let (lines, _) = broadcast::channel(1024);
    let (state_tx, state) = watch::channel(JobState::Running {
        pid: i64::from(std::process::id()),
    });
    let (done_tx, done) = oneshot::channel();
    let task = {
        let shared = Arc::clone(&shared);
        let lines = lines.clone();
        tokio::spawn(async move {
            let mut guard = session.lock_owned().await;
            let (tx, mut rx) = tokio::sync::mpsc::channel::<nori_bash::OutputLine>(256);
            let options = nori_bash::ExecOptions {
                timeout_ms: Some(JOB_LIMIT_MS),
                cancel: Some(shared.cancel.clone()),
            };
            let execution = guard.exec_streaming(&command, options, tx);
            tokio::pin!(execution);
            let take = |line: nori_bash::OutputLine| {
                let mut log = shared.log.lock().unwrap_or_else(PoisonError::into_inner);
                let (stream, after) = if line.stderr {
                    let end = log.err.push(&line.text);
                    (
                        Stream::Stderr,
                        Cursor {
                            stdout: log.out.end(),
                            stderr: end,
                        },
                    )
                } else {
                    let end = log.out.push(&line.text);
                    (
                        Stream::Stdout,
                        Cursor {
                            stdout: end,
                            stderr: log.err.end(),
                        },
                    )
                };
                log.seq += 1;
                let _ = lines.send(JobLine {
                    stream,
                    text: line.text,
                    after,
                    seq: log.seq,
                });
            };
            let result = loop {
                tokio::select! {
                    Some(line) = rx.recv() => take(line),
                    result = &mut execution => break result,
                }
            };
            while let Ok(line) = rx.try_recv() {
                take(line);
            }
            let end = match &result {
                Ok(output) => JobState::Exited {
                    code: output.exit_code,
                },
                Err(nori_bash::BashError::Cancelled { .. }) => JobState::Killed {
                    signal: "TERM".to_owned(),
                },
                Err(nori_bash::BashError::Timeout { .. }) => JobState::Killed {
                    signal: "timeout".to_owned(),
                },
                Err(error) => JobState::Lost {
                    reason: error.to_string(),
                },
            };
            // Output was streamed, not collected: give the waiter what was kept.
            let result = result.map(|mut output| {
                let log = shared.log.lock().unwrap_or_else(PoisonError::into_inner);
                output.stdout = String::from_utf8_lossy(&log.out.bytes).into_owned();
                output.stderr = String::from_utf8_lossy(&log.err.bytes).into_owned();
                output
            });
            state_tx.send_replace(end);
            let _ = done_tx.send(result);
        })
        .abort_handle()
    };
    Running {
        done,
        shared,
        lines,
        state,
        task,
    }
}

impl Running {
    /// What it printed so far, for a result that says it is still running.
    pub(super) fn output_so_far(&self) -> (String, String) {
        let log = self
            .shared
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (
            String::from_utf8_lossy(&log.out.bytes).into_owned(),
            String::from_utf8_lossy(&log.err.bytes).into_owned(),
        )
    }

    /// Follow it as a job from here on.
    pub(super) fn into_job(self, label: String, command: String) -> JobHandle {
        let control = Arc::new(Control {
            shared: Arc::clone(&self.shared),
        });
        JobHandle::new(
            JobRecord::in_process(label, command),
            self.lines,
            self.state,
            control,
            self.task,
        )
    }
}

struct Control {
    shared: Arc<Shared>,
}

impl JobControl for Control {
    fn read(&self, from: ReadFrom, max_bytes: i64) -> BoxFuture<'_, Result<JobOutput, JobError>> {
        let log = self
            .shared
            .log
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let start = match from {
            ReadFrom::Cursor(cursor) => cursor,
            ReadFrom::Everything => Cursor::default(),
            ReadFrom::Tail { bytes } => Cursor {
                stdout: (log.out.end() - bytes).max(0),
                stderr: (log.err.end() - bytes).max(0),
            },
        };
        let (out, out_next) = log.out.lines_from(start.stdout, max_bytes);
        let (err, err_next) = log.err.lines_from(start.stderr, max_bytes);
        let next = Cursor {
            stdout: out_next,
            stderr: err_next,
        };
        let lines = out
            .into_iter()
            .map(|text| (Stream::Stdout, text))
            .chain(err.into_iter().map(|text| (Stream::Stderr, text)))
            .enumerate()
            .map(|(index, (stream, text))| JobLine {
                stream,
                text,
                after: next,
                seq: index as i64 + 1,
            })
            .collect();
        drop(log);
        Box::pin(async move { Ok(JobOutput { lines, next }) })
    }

    fn signal(&self, _signal: Signal) -> BoxFuture<'_, Result<(), JobError>> {
        // An in-process command takes one signal: stop.
        self.shared.cancel.cancel();
        Box::pin(async { Ok(()) })
    }

    fn stop(&self, _grace: Duration) -> BoxFuture<'_, Result<(), JobError>> {
        self.shared.cancel.cancel();
        Box::pin(async { Ok(()) })
    }
}

/// What a shell's state is taken from before a command that may outlive its timeout.
pub(super) const SNAPSHOT: &str = "export -p; alias -p; declare -f";

/// What a fresh shell took over from the one a command kept.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(super) struct Carried {
    pub(super) exports: i64,
    pub(super) aliases: i64,
    pub(super) functions: i64,
}

/// Replay `snapshot` (the output of [`SNAPSHOT`]) into `fresh`.
pub(super) async fn restore(fresh: &mut nori_bash::BashSession, snapshot: Option<&str>) -> Carried {
    let Some(snapshot) = snapshot.filter(|text| !text.trim().is_empty()) else {
        return Carried::default();
    };
    let carried = Carried {
        exports: snapshot
            .lines()
            .filter(|line| line.starts_with("declare -x") || line.starts_with("export "))
            .count() as i64,
        aliases: snapshot
            .lines()
            .filter(|line| line.starts_with("alias "))
            .count() as i64,
        functions: snapshot
            .lines()
            .filter(|line| line.trim_end().ends_with("()") && !line.starts_with(' '))
            .count() as i64,
    };
    let options = nori_bash::ExecOptions {
        timeout_ms: Some(5_000),
        cancel: None,
    };
    match fresh.exec_with_options(snapshot, options).await {
        Ok(output) if output.exit_code == 0 => carried,
        Ok(_) | Err(_) => Carried::default(),
    }
}

/// How much of each stream a result shows of a command that is still running or just ended.
pub(super) const SO_FAR_BYTES: i64 = 4 * 1024 * 1024;

/// A job's output as two streams of text.
pub(super) fn streams(output: &nori_machines::jobs::JobOutput) -> (String, String) {
    let mut stdout = String::new();
    let mut stderr = String::new();
    for line in &output.lines {
        let stream = match line.stream {
            Stream::Stdout => &mut stdout,
            Stream::Stderr => &mut stderr,
        };
        stream.push_str(&line.text);
        stream.push('\n');
    }
    (stdout, stderr)
}
