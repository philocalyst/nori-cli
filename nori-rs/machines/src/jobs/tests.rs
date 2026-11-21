//! Jobs on this computer, through the same scripts a remote machine runs.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use pretty_assertions::assert_eq;

use super::Cursor;
use super::JobSpec;
use super::JobState;
use super::MachineSupervisor;
use super::Place;
use super::ReadFrom;
use super::Supervisor;
use crate::Roster;
use crate::StartDir;
use crate::Stream;

fn supervisor(root: &std::path::Path) -> MachineSupervisor {
    MachineSupervisor::with_root(Arc::new(Roster::empty()), &root.display().to_string())
}

fn spec(command: &str) -> JobSpec {
    JobSpec {
        command: command.to_owned(),
        place: Place::Local,
        start: StartDir::FirstOf(Vec::new()),
        env: BTreeMap::from([("NORI_TEST_GREETING".to_owned(), "it's me".to_owned())]),
        label: "test".to_owned(),
    }
}

fn temp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap_or_else(|error| unreachable!("a temporary directory: {error}"))
}

#[tokio::test]
async fn a_job_streams_its_lines_and_reports_how_it_ended() {
    let root = temp();
    let supervisor = supervisor(root.path());
    let job = supervisor
        .start(spec(
            "cd /; echo \"$NORI_TEST_GREETING\"; echo two >&2; sleep 0.3; printf three; exit 4",
        ))
        .await
        .unwrap_or_else(|error| unreachable!("started: {error}"));
    let mut lines = job.lines();
    let state = job.wait(Duration::from_secs(10)).await;
    assert_eq!(state, JobState::Exited { code: 4 });
    // Where it ended, for the next command to start there.
    assert_eq!(job.final_cwd().as_deref(), Some("/"));
    let mut seen = Vec::new();
    while let Ok(line) = lines.try_recv() {
        seen.push((line.stream, line.text));
    }
    seen.sort_by_key(|(stream, _)| *stream == Stream::Stderr);
    assert_eq!(
        seen,
        vec![
            (Stream::Stdout, "it's me".to_owned()),
            (Stream::Stdout, "three".to_owned()),
            (Stream::Stderr, "two".to_owned()),
        ]
    );
    // Everything is still there to read, exactly, from any cursor.
    let output = job
        .read(ReadFrom::Cursor(Cursor::default()), 1 << 20)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    let texts: Vec<_> = output.lines.iter().map(|line| line.text.as_str()).collect();
    assert_eq!(texts, vec!["it's me", "two"]);
    // Everything includes the last line, which has no newline.
    let everything = job
        .read(ReadFrom::Everything, 1 << 20)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    let texts: Vec<_> = everything
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect();
    assert_eq!(texts, vec!["it's me", "three", "two"]);
    assert_eq!(
        output.next,
        Cursor {
            stdout: 8,
            stderr: 4
        }
    );
}

#[tokio::test]
async fn a_job_outlives_its_handle_is_adopted_again_and_stops_on_request() {
    let root = temp();
    let supervisor = supervisor(root.path());
    let job = supervisor
        .start(spec("echo started; sleep 30; echo never"))
        .await
        .unwrap_or_else(|error| unreachable!("started: {error}"));
    let record = job.record().clone();
    drop(job);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let adopted = supervisor
        .adopt(record.clone())
        .await
        .unwrap_or_else(|error| unreachable!("adopted: {error}"));
    assert_eq!(
        *adopted.state().borrow(),
        JobState::Running { pid: record.pid }
    );
    let tail = adopted
        .read(ReadFrom::Tail { bytes: 1024 }, 1024)
        .await
        .unwrap_or_else(|error| unreachable!("read: {error}"));
    // A tail drops the line it may start in; this one starts at the file's beginning.
    assert!(tail.lines.len() <= 1);
    adopted
        .stop(Duration::from_millis(500))
        .await
        .unwrap_or_else(|error| unreachable!("stopped: {error}"));
    let state = adopted.wait(Duration::from_secs(10)).await;
    assert_eq!(
        state,
        JobState::Killed {
            signal: "TERM".to_owned()
        }
    );
}
