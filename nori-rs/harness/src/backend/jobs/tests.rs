//! The registry with a scripted supervisor: the read cursor, labels, and what a stop does.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

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
use nori_machines::jobs::Place;
use nori_machines::jobs::ReadFrom;
use nori_machines::jobs::Signal;
use nori_machines::jobs::Supervisor;
use pretty_assertions::assert_eq;
use tokio::sync::broadcast;
use tokio::sync::watch;

use super::JobOrigin;
use super::JobWake;
use super::Jobs;
use super::ReadSpec;
use crate::backend::monitor::Monitors;

/// A job whose stdout is a list of lines the test appends to.
struct Scripted {
    stdout: Mutex<Vec<String>>,
    state: watch::Sender<JobState>,
}

impl JobControl for Scripted {
    fn read(&self, from: ReadFrom, max_bytes: i64) -> BoxFuture<'_, Result<JobOutput, JobError>> {
        Box::pin(async move {
            let at = match from {
                ReadFrom::Cursor(cursor) => cursor.stdout,
                ReadFrom::Tail { bytes } => -bytes,
                ReadFrom::Everything => 0,
            };
            let stdout = self
                .stdout
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut output = JobOutput {
                lines: Vec::new(),
                next: Cursor {
                    stdout: at.max(0),
                    stderr: 0,
                },
            };
            let mut offset = 0;
            for text in stdout.iter() {
                let end = offset + text.len() as i64 + 1;
                // Whole lines that start at or after the cursor, at most `max_bytes` of them.
                if offset >= at.max(0) && end - at.max(0) <= max_bytes {
                    output.next.stdout = end;
                    output.lines.push(JobLine {
                        stream: Stream::Stdout,
                        text: text.clone(),
                        after: output.next,
                        seq: 0,
                    });
                }
                offset = end;
            }
            Ok(output)
        })
    }

    fn signal(&self, _: Signal) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async { Ok(()) })
    }

    fn stop(&self, _: Duration) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            self.state.send_replace(JobState::Killed {
                signal: "TERM".to_owned(),
            });
            Ok(())
        })
    }
}

fn handle(label: &str, lines: &[&str]) -> (JobHandle, Arc<Scripted>) {
    let record = JobRecord {
        id: JobId::fresh(),
        place: Place::Local,
        label: label.to_owned(),
        command: "serve".to_owned(),
        pid: 4242,
        boot: "boot".to_owned(),
        started_at: 0,
    };
    let (lines_tx, _) = broadcast::channel::<JobLine>(16);
    let (state_tx, state_rx) = watch::channel(JobState::Running { pid: 4242 });
    let control = Arc::new(Scripted {
        stdout: Mutex::new(lines.iter().map(ToString::to_string).collect()),
        state: state_tx,
    });
    let follower = tokio::spawn(std::future::pending::<()>()).abort_handle();
    (
        JobHandle::new(record, lines_tx, state_rx, control.clone(), follower),
        control,
    )
}

fn jobs() -> Jobs {
    Jobs::new(Monitors::new(), std::path::PathBuf::from("/nowhere"))
}

fn quiet() -> JobWake {
    JobWake {
        on_exit: false,
        on_output: None,
        ready: None,
    }
}

#[tokio::test]
async fn a_new_read_returns_each_line_once_and_other_reads_do_not_move_it_back() {
    let jobs = jobs();
    let (handle, control) = handle("dev", &["one", "two"]);
    jobs.adopt_handle(handle, JobOrigin::Agent, quiet())
        .await
        .unwrap_or_else(|error| unreachable!("adopted: {error}"));
    let first = jobs
        .read("dev", ReadSpec::New, 4096)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!(
        (first.stdout.as_str(), first.lines, first.more),
        ("one\ntwo", 2, false)
    );
    let again = jobs
        .read("dev", ReadSpec::New, 4096)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!((again.stdout.as_str(), again.lines), ("", 0));
    control
        .stdout
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push("three".to_owned());
    let more = jobs
        .read("dev", ReadSpec::New, 4096)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!(more.stdout, "three");
    // Reading from the start does not give the lines back to `new`.
    let all = jobs
        .read("dev", ReadSpec::Start, 4096)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!(all.stdout, "one\ntwo\nthree");
    let after = jobs
        .read("dev", ReadSpec::New, 4096)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!(after.lines, 0);
    assert_eq!(
        after.cursor,
        Cursor {
            stdout: 14,
            stderr: 0
        }
    );
}

#[tokio::test]
async fn a_read_that_fills_its_limit_says_more_is_waiting() {
    let jobs = jobs();
    let long = "x".repeat(1000);
    let lines: Vec<&str> = std::iter::repeat_n(long.as_str(), 12).collect();
    let (handle, _control) = handle("noisy", &lines);
    jobs.adopt_handle(handle, JobOrigin::Agent, quiet())
        .await
        .unwrap_or_else(|error| unreachable!("adopted: {error}"));
    // The smallest read is 8 KiB: eight whole lines of the twelve.
    let first = jobs
        .read("noisy", ReadSpec::New, 1)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!((first.lines, first.more), (8, true));
    let rest = jobs
        .read("noisy", ReadSpec::New, 1)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    assert_eq!((rest.lines, rest.more), (4, false));
}

#[tokio::test]
async fn labels_are_unique_among_running_jobs_and_a_stop_ends_one() {
    let jobs = jobs();
    let (first, _) = handle("dev", &[]);
    let (second, _) = handle("dev", &[]);
    let one = jobs
        .adopt_handle(first, JobOrigin::Agent, quiet())
        .await
        .unwrap_or_else(|error| unreachable!("adopted: {error}"));
    let two = jobs
        .adopt_handle(second, JobOrigin::Bash { call_id: None }, quiet())
        .await
        .unwrap_or_else(|error| unreachable!("adopted: {error}"));
    assert_eq!((one.label.as_str(), two.label.as_str()), ("dev", "dev-2"));
    assert_eq!(jobs.summary().running, 2);
    let stopped = jobs
        .stop("dev-2", None, Duration::from_millis(10))
        .await
        .unwrap_or_else(|error| unreachable!("stopped: {error}"));
    assert_eq!(stopped.state, "killed by TERM");
    // The watcher sees the end shortly after.
    for _ in 0..50 {
        if jobs.summary().running == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(jobs.summary().running, 1);
    let error = jobs
        .stop("dev-2", None, Duration::ZERO)
        .await
        .err()
        .unwrap_or_default();
    assert!(error.contains("is not running"), "{error}");
    let error = jobs
        .read("nope", ReadSpec::New, 100)
        .await
        .err()
        .unwrap_or_default();
    assert!(error.contains("`dev`"), "{error}");
}

#[tokio::test]
async fn an_installed_supervisor_starts_the_jobs() {
    struct Fixed(Mutex<Option<JobHandle>>);
    impl Supervisor for Fixed {
        fn start(&self, spec: JobSpec) -> BoxFuture<'_, Result<JobHandle, JobError>> {
            let handle = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            Box::pin(async move {
                handle.ok_or(JobError::Launch {
                    place: spec.place,
                    reason: "no more".to_owned(),
                })
            })
        }
        fn adopt(&self, _: JobRecord) -> BoxFuture<'_, Result<JobHandle, JobError>> {
            Box::pin(async {
                Err(JobError::Lost {
                    reason: "none".to_owned(),
                })
            })
        }
    }
    let jobs = jobs();
    let (handle, _) = handle("scripted", &["hello"]);
    jobs.install_supervisor(Arc::new(Fixed(Mutex::new(Some(handle)))));
    let summary = jobs
        .start(super::StartJob {
            command: "serve".to_owned(),
            label: Some("scripted".to_owned()),
            machine: None,
            cwd: None,
            env: Default::default(),
            origin: JobOrigin::Agent,
            wake: quiet(),
            local_cwd: std::env::temp_dir(),
        })
        .await
        .unwrap_or_else(|error| unreachable!("started: {error}"));
    assert_eq!(
        (summary.label.as_str(), summary.state.as_str(), summary.pid),
        ("scripted", "running", 4242)
    );
    let error = jobs
        .start(super::StartJob {
            command: "serve".to_owned(),
            label: Some("scripted".to_owned()),
            machine: None,
            cwd: None,
            env: Default::default(),
            origin: JobOrigin::Agent,
            wake: quiet(),
            local_cwd: std::env::temp_dir(),
        })
        .await
        .err()
        .unwrap_or_default();
    assert!(error.contains("already running"), "{error}");
}
