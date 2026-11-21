//! Running a script on a machine over ssh, with its output streamed back.
//!
//! Why ssh and not Rex: Rex reaches a host to show its terminals, and its API has no way to run a
//! command without one: `rex -S HOST run --wait` opens a pane on that host, its output can only be
//! read back as a captured screen (wrapped, stdout and stderr merged), and the exit status takes
//! another call. ssh gives two byte streams, the exact exit status, `BatchMode` (never a prompt),
//! and connection sharing. Rex's own ssh hosts are ssh endpoints in the first place.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::MachineName;
use crate::SshDestination;
use crate::script;
use crate::script::Report;

/// How long ssh may take to connect before the machine counts as unreachable.
const CONNECT_TIMEOUT_SECONDS: i64 = 8;
/// After Nori hangs up on a command, how long its machine has to end it.
const HANG_UP_GRACE: Duration = Duration::from_secs(4);
/// How much of each stream a run keeps: this much of its start and this much of its end.
const KEEP_BYTES: usize = 256 * 1024;

/// Which stream a line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One line of output, as it arrives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub stream: Stream,
    pub text: String,
}

/// A finished command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
    /// Where it started.
    pub started_in: String,
    /// Where it ended: where the next command on this machine starts.
    pub cwd: String,
    pub duration_ms: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error(
        "{machine} is unreachable over ssh ({destination}): {reason}. The command did not run."
    )]
    Unreachable {
        machine: MachineName,
        destination: SshDestination,
        reason: String,
    },
    #[error("there is no directory {dir} on {machine}. The command did not run.")]
    NoSuchDirectory { machine: MachineName, dir: String },
    #[error("timed out after {}s on {machine} (the command and everything it started there were stopped)", .after_ms / 1000)]
    Timeout {
        machine: MachineName,
        after_ms: i64,
        stdout: String,
        stderr: String,
    },
    #[error(
        "the connection to {machine} was lost while the command ran ({reason}); it was stopped there"
    )]
    ConnectionLost {
        machine: MachineName,
        reason: String,
        stdout: String,
        stderr: String,
    },
    #[error("could not start ssh: {0}")]
    Spawn(#[from] std::io::Error),
}

/// A script that keeps running: read its stdout; drop it to hang up.
pub(crate) struct Streaming {
    pub(crate) child: tokio::process::Child,
    /// Held open while the script runs: the machine's end of file is Nori hanging up.
    _lifeline: Option<tokio::process::ChildStdin>,
}

/// How a connection reaches the machine.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Via<'a> {
    Ssh {
        destination: &'a SshDestination,
        /// Where shared connections live, when Nori shares them.
        control_dir: Option<&'a Path>,
    },
    /// `/bin/sh -s` on this computer: the same scripts and the same reading of them, without a
    /// network. Jobs on this computer run this way, and tests prove what a machine would do.
    Local,
}

/// One connection to a machine.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Connection<'a> {
    pub(crate) machine: &'a MachineName,
    pub(crate) via: Via<'a>,
}

impl Connection<'_> {
    fn command(&self) -> Command {
        let mut command = match self.via {
            Via::Ssh {
                destination,
                control_dir,
            } => ssh_command(destination, control_dir),
            Via::Local => {
                let mut command = Command::new("/bin/sh");
                // A group of its own, as sshd gives a session: the watchdog kills its group.
                command.arg("-s").process_group(0);
                command
            }
        };
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    fn destination(&self) -> SshDestination {
        match self.via {
            Via::Ssh { destination, .. } => destination.clone(),
            Via::Local => SshDestination::this_computer(),
        }
    }
}

/// `ssh` that never prompts, gives up connecting after a few seconds, notices a dead link, and
/// says nothing of its own but errors.
fn ssh_command(destination: &SshDestination, control_dir: Option<&Path>) -> Command {
    let mut command = Command::new("ssh");
    command.args([
        "-T",
        "-e",
        "none",
        "-o",
        "BatchMode=yes",
        "-o",
        &format!("ConnectTimeout={CONNECT_TIMEOUT_SECONDS}"),
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=4",
        "-o",
        "LogLevel=ERROR",
    ]);
    if let Some(dir) = control_dir {
        command.args([
            "-o".to_owned(),
            "ControlMaster=auto".to_owned(),
            "-o".to_owned(),
            format!("ControlPath={}/%C", dir.display()),
            "-o".to_owned(),
            "ControlPersist=120".to_owned(),
        ]);
    }
    command.arg(destination.as_str()).args(["/bin/sh", "-s"]);
    command
}

impl Connection<'_> {
    fn unreachable(&self, reason: String) -> ExecError {
        ExecError::Unreachable {
            machine: self.machine.clone(),
            destination: self.destination(),
            reason,
        }
    }

    /// Run the probe script and return what it printed.
    pub(crate) async fn probe(&self) -> Result<String, ExecError> {
        let stdout = self
            .run_short(
                script::PROBE,
                Duration::from_secs(CONNECT_TIMEOUT_SECONDS as u64 + 20),
            )
            .await?;
        if stdout.starts_with("nori-probe") {
            Ok(stdout)
        } else {
            Err(self.unreachable("it answered, but not as a POSIX shell".to_owned()))
        }
    }

    /// Run a short script to its end and return its stdout. A machine that does not answer
    /// within `limit`, or whose shell fails, is unreachable.
    pub(crate) async fn run_short(
        &self,
        script: &str,
        limit: Duration,
    ) -> Result<String, ExecError> {
        let stdout = self.run_short_bytes(script, limit).await?;
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    }

    /// [`Self::run_short`], with stdout as bytes.
    pub(crate) async fn run_short_bytes(
        &self,
        script: &str,
        limit: Duration,
    ) -> Result<Vec<u8>, ExecError> {
        let mut child = self.command().spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            // A machine that hangs up early is reported by its exit, below.
            let _ = stdin.write_all(script.as_bytes()).await;
        }
        let output = match tokio::time::timeout(limit, child.wait_with_output()).await {
            Ok(output) => output?,
            Err(_) => {
                return Err(
                    self.unreachable(format!("it did not answer within {}s", limit.as_secs()))
                );
            }
        };
        if output.status.success() {
            return Ok(output.stdout);
        }
        Err(self.unreachable(reason_from(&output.stderr, output.status.code())))
    }

    /// Start a long-running script whose stdout the caller reads. Dropping the returned
    /// stdin (or the child) hangs up.
    pub(crate) async fn spawn_stream(&self, script: &str) -> Result<Streaming, ExecError> {
        let mut child = self.command().spawn()?;
        let mut stdin = child.stdin.take();
        if let Some(pipe) = stdin.as_mut() {
            let _ = pipe.write_all(script.as_bytes()).await;
            let _ = pipe.flush().await;
        }
        Ok(Streaming {
            child,
            _lifeline: stdin,
        })
    }

    /// Run `command`, streaming its lines to `lines` while it runs.
    pub(crate) async fn exec(
        &self,
        script: String,
        timeout: Option<Duration>,
        lines: Option<mpsc::Sender<Line>>,
    ) -> Result<Run, ExecError> {
        let started = Instant::now();
        let mut child = self.command().spawn()?;
        // Held open while the command runs: closing it is how Nori hangs up.
        let mut stdin = child.stdin.take();
        if let Some(pipe) = stdin.as_mut() {
            let _ = pipe.write_all(script.as_bytes()).await;
            let _ = pipe.flush().await;
        }
        let (tx, mut rx) = mpsc::channel::<Line>(256);
        spawn_reader(child.stdout.take(), Stream::Stdout, tx.clone());
        spawn_reader(child.stderr.take(), Stream::Stderr, tx);

        let mut state = RunState::default();
        let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
        let mut timed_out = false;
        let status = loop {
            let sleep = async {
                match deadline {
                    Some(deadline) if !timed_out => tokio::time::sleep_until(deadline).await,
                    Some(_) | None => std::future::pending().await,
                }
            };
            tokio::select! {
                Some(line) = rx.recv() => state.take(line, lines.as_ref()).await,
                status = child.wait() => break status?,
                () = sleep => {
                    timed_out = true;
                    // Hang up: the machine's watchdog ends the command and everything it started.
                    stdin = None;
                    if tokio::time::timeout(HANG_UP_GRACE, child.wait()).await.is_err() {
                        let _ = child.start_kill();
                    }
                }
            }
        };
        drop(stdin);
        // Lines still in flight; a stray process holding a pipe cannot hold the result.
        let drain = async {
            while let Some(line) = rx.recv().await {
                state.take(line, lines.as_ref()).await;
            }
        };
        let _ = tokio::time::timeout(Duration::from_millis(500), drain).await;
        let duration_ms = started.elapsed().as_millis() as i64;
        let code = status.code();
        if timed_out {
            return Err(ExecError::Timeout {
                machine: self.machine.clone(),
                after_ms: timeout.map_or(0, |timeout| timeout.as_millis() as i64),
                stdout: state.stdout.finish(),
                stderr: state.stderr.finish(),
            });
        }
        if let Some(dir) = state.no_dir {
            return Err(ExecError::NoSuchDirectory {
                machine: self.machine.clone(),
                dir,
            });
        }
        let Some(started_in) = state.started_in else {
            return Err(self.unreachable(reason_from(state.stderr.finish().as_bytes(), code)));
        };
        match state.ended {
            Some((exit_code, cwd)) => Ok(Run {
                exit_code,
                stdout: state.stdout.finish(),
                stderr: state.stderr.finish(),
                cwd: if cwd.is_empty() {
                    started_in.clone()
                } else {
                    cwd
                },
                started_in,
                duration_ms,
            }),
            None => Err(ExecError::ConnectionLost {
                machine: self.machine.clone(),
                reason: match code {
                    Some(255) | None => "ssh disconnected".to_owned(),
                    Some(code) => format!("the remote shell exited with {code}"),
                },
                stdout: state.stdout.finish(),
                stderr: state.stderr.finish(),
            }),
        }
    }
}

/// Why ssh did not run anything: its own last words, or its exit status.
fn reason_from(stderr: &[u8], code: Option<i32>) -> String {
    let text = String::from_utf8_lossy(stderr);
    let reason = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.contains(script::MARK))
        .collect::<Vec<_>>()
        .join("; ");
    match (reason.is_empty(), code) {
        (false, _) => reason.trim_start_matches("ssh: ").to_owned(),
        (true, Some(code)) => format!("ssh exited with {code}"),
        (true, None) => "ssh was killed".to_owned(),
    }
}

fn spawn_reader<R>(pipe: Option<R>, stream: Stream, tx: mpsc::Sender<Line>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let Some(pipe) = pipe else {
        return;
    };
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            match reader.read_until(b'\n', &mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&buffer).into_owned();
                    if tx.send(Line { stream, text }).await.is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// What a run has seen so far.
#[derive(Default)]
struct RunState {
    stdout: Kept,
    stderr: Kept,
    started_in: Option<String>,
    no_dir: Option<String>,
    ended: Option<(i64, String)>,
}

impl RunState {
    /// One raw line, newline included when it had one.
    async fn take(&mut self, line: Line, lines: Option<&mpsc::Sender<Line>>) {
        let (text, report) = match line.stream {
            Stream::Stdout => (Some(line.text.as_str()), None),
            Stream::Stderr => {
                let (text, report) = script::split_report(line.text.trim_end_matches('\n'));
                // A line that only carried a report adds nothing to stderr.
                let text = match (text, report.is_some()) {
                    (Some(text), true) => Some(text),
                    (Some(_), false) => Some(line.text.as_str()),
                    (None, _) => None,
                };
                (text, report)
            }
        };
        if let Some(text) = text {
            match line.stream {
                Stream::Stdout => self.stdout.push(text),
                Stream::Stderr => self.stderr.push(text),
            }
            if let Some(lines) = lines {
                let _ = lines
                    .send(Line {
                        stream: line.stream,
                        text: text.trim_end_matches(['\n', '\r']).to_owned(),
                    })
                    .await;
            }
        }
        match report {
            Some(Report::Start { cwd }) => self.started_in = Some(cwd),
            Some(Report::NoDir { dir }) => self.no_dir = Some(dir),
            Some(Report::End { status, cwd }) => self.ended = Some((status, cwd)),
            None => {}
        }
    }
}

/// A stream as kept: its start, and a rolling window of its end.
#[derive(Default)]
struct Kept {
    head: String,
    tail: std::collections::VecDeque<String>,
    tail_bytes: usize,
    omitted: usize,
}

impl Kept {
    fn push(&mut self, text: &str) {
        if self.head.len() + text.len() <= KEEP_BYTES && self.tail.is_empty() {
            self.head.push_str(text);
            return;
        }
        self.tail_bytes += text.len();
        self.tail.push_back(text.to_owned());
        while self.tail_bytes > KEEP_BYTES {
            let Some(dropped) = self.tail.pop_front() else {
                break;
            };
            self.tail_bytes -= dropped.len();
            self.omitted += dropped.len();
        }
    }

    fn finish(&mut self) -> String {
        let mut text = std::mem::take(&mut self.head);
        if self.omitted > 0 {
            text.push_str(&format!("\n[... {} bytes omitted ...]\n", self.omitted));
        }
        for part in self.tail.drain(..) {
            text.push_str(&part);
        }
        text
    }
}
