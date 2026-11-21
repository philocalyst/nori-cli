//! What a machine does with Nori's scripts, proven with `/bin/sh` on this computer: the same
//! script and the same reading of its output, without the network in between.

use std::time::Duration;

use pretty_assertions::assert_eq;
use tokio::sync::mpsc;

use crate::ExecError;
use crate::Line;
use crate::MachineName;
use crate::RemoteDir;
use crate::Run;
use crate::StartDir;
use crate::Stream;
use crate::exec::Connection;
use crate::exec::Via;
use crate::script::ExecScript;

fn machine() -> MachineName {
    "test-box"
        .parse()
        .unwrap_or_else(|_| unreachable!("a valid name"))
}

async fn run(
    command: &str,
    start: StartDir,
    timeout: Option<Duration>,
    lines: Option<mpsc::Sender<Line>>,
) -> Result<Run, ExecError> {
    let name = machine();
    let script = ExecScript {
        command,
        start: &start,
        path: None,
    }
    .render();
    Connection {
        machine: &name,
        via: Via::Local,
    }
    .exec(script, timeout, lines)
    .await
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap_or_else(|error| unreachable!("a temporary directory: {error}"))
}

#[tokio::test]
async fn a_command_reports_its_streams_status_and_where_it_ended() {
    let dir = temp_dir();
    let inner = dir.path().join("inner");
    std::fs::create_dir(&inner).unwrap_or_else(|error| unreachable!("{error}"));
    let start = StartDir::Required(RemoteDir::Path(dir.path().display().to_string()));
    let command = "cd inner && printf 'out\\n' && printf 'err\\n' >&2 && exit 3";
    let run = run(command, start, None, None).await.map(|run| {
        (
            run.exit_code,
            run.stdout,
            run.stderr,
            run.cwd == inner.display().to_string(),
        )
    });
    assert_eq!(
        run.map_err(|error| error.to_string()),
        Ok((3, "out\n".to_owned(), "err\n".to_owned(), true))
    );
}

#[tokio::test]
async fn any_text_survives_the_trip() {
    let command = "x='it'\"'\"'s $HOME'; cat <<'EOF'\n}\n$x\nEOF\nprintf '%s' \"$x\"";
    let run = run(command, StartDir::FirstOf(Vec::new()), None, None).await;
    assert_eq!(
        run.map(|run| run.stdout).map_err(|error| error.to_string()),
        Ok("}\n$x\nit's $HOME".to_owned())
    );
}

#[tokio::test]
async fn a_required_directory_that_is_missing_runs_nothing() {
    let start = StartDir::Required(RemoteDir::Path("/no/such/nori/dir".to_owned()));
    let run = run("echo ran", start, None, None).await;
    assert!(
        matches!(run, Err(ExecError::NoSuchDirectory { ref dir, .. }) if dir == "/no/such/nori/dir"),
        "{run:?}"
    );
}

#[tokio::test]
async fn candidates_start_in_the_first_that_exists() {
    let dir = temp_dir();
    let start = StartDir::FirstOf(vec![
        RemoteDir::Path("/no/such/nori/dir".to_owned()),
        RemoteDir::Path(dir.path().display().to_string()),
    ]);
    let run = run("pwd", start, None, None).await;
    assert_eq!(
        run.map(|run| (run.started_in.clone(), run.stdout.trim().to_owned()))
            .map_err(|error| error.to_string()),
        Ok((
            dir.path().display().to_string(),
            dir.path().display().to_string()
        ))
    );
}

#[tokio::test]
async fn lines_stream_while_it_runs() {
    let (tx, mut rx) = mpsc::channel(16);
    let run = run(
        "echo one; echo two >&2; printf three",
        StartDir::FirstOf(Vec::new()),
        None,
        Some(tx),
    )
    .await;
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line);
    }
    assert!(run.is_ok(), "{run:?}");
    lines.sort_by_key(|line| line.stream == Stream::Stderr);
    assert_eq!(
        lines,
        vec![
            Line {
                stream: Stream::Stdout,
                text: "one".to_owned()
            },
            Line {
                stream: Stream::Stdout,
                text: "three".to_owned()
            },
            Line {
                stream: Stream::Stderr,
                text: "two".to_owned()
            },
        ]
    );
}

#[tokio::test]
async fn a_timeout_hangs_up_and_the_machine_stops_everything_it_started() {
    let dir = temp_dir();
    let pid_file = dir.path().join("pid");
    let command = format!(
        "sh -c 'echo $$ > {}; exec sleep 30' & wait",
        pid_file.display()
    );
    let started = std::time::Instant::now();
    let run = run(
        &command,
        StartDir::FirstOf(Vec::new()),
        Some(Duration::from_millis(800)),
        None,
    )
    .await;
    assert!(matches!(run, Err(ExecError::Timeout { .. })), "{run:?}");
    assert!(started.elapsed() < Duration::from_secs(8));
    let pid = std::fs::read_to_string(&pid_file).unwrap_or_default();
    let pid = pid.trim();
    assert!(!pid.is_empty());
    // The sleep was stopped, not orphaned.
    let mut alive = true;
    for _ in 0..30 {
        alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .is_ok_and(|status| status.success());
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "process {pid} outlived the hang-up");
}
