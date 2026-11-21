//! The supervisor for real machines: this computer through `/bin/sh`, others through ssh.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::BufReader;
use tokio::sync::broadcast;
use tokio::sync::watch;

use super::BoxFuture;
use super::Cursor;
use super::JobControl;
use super::JobError;
use super::JobHandle;
use super::JobId;
use super::JobLine;
use super::JobOutput;
use super::JobRecord;
use super::JobSpec;
use super::JobState;
use super::Place;
use super::ReadFrom;
use super::Signal;
use super::Supervisor;
use super::script;
use crate::Facts;
use crate::Machine;
use crate::Roster;
use crate::Stream;
use crate::Target;
use crate::exec::Connection;
use crate::exec::Via;
use crate::script::MARK;

/// Short scripts (start, stop, check, read) must answer within this.
const SHORT: Duration = Duration::from_secs(30);
/// Live lines kept for receivers that fall behind.
const LINE_BUFFER: usize = 1024;

/// Jobs on this computer and on the roster's machines.
pub struct MachineSupervisor {
    /// The roster it uses; `None`: this process's, as discovered ([`crate::global_ready`]).
    roster: Option<Arc<Roster>>,
    /// Where jobs live on every machine, as a shell word.
    root: Arc<str>,
}

impl MachineSupervisor {
    /// Jobs in `~/.nori/jobs` on each machine of `roster`.
    pub fn new(roster: Arc<Roster>) -> Self {
        Self {
            roster: Some(roster),
            root: Arc::from(script::DEFAULT_ROOT),
        }
    }

    /// Jobs in `~/.nori/jobs` on each machine of this process's roster, waited for (never
    /// blocking) when a job starts or is adopted.
    pub fn of_this_process() -> Self {
        Self {
            roster: None,
            root: Arc::from(script::DEFAULT_ROOT),
        }
    }

    /// Jobs under `root` (an absolute path) on each machine.
    pub fn with_root(roster: Arc<Roster>, root: &str) -> Self {
        Self {
            roster: Some(roster),
            root: Arc::from(crate::script::quote(root)),
        }
    }

    async fn roster(&self) -> Arc<Roster> {
        match &self.roster {
            Some(roster) => Arc::clone(roster),
            None => crate::global_ready().await,
        }
    }
}

/// How to reach a job's machine, owned so follower tasks can keep it.
#[derive(Clone)]
struct Reach {
    roster: Arc<Roster>,
    machine: Option<Machine>,
    root: Arc<str>,
}

impl Reach {
    fn new(roster: &Arc<Roster>, root: &Arc<str>, place: &Place) -> Result<Self, JobError> {
        let machine = match place {
            Place::Local => None,
            Place::Machine(name) => match roster.resolve(Some(name.as_str()))? {
                Target::Local => None,
                Target::Remote(machine) => Some(machine.clone()),
            },
        };
        // A machine may keep its jobs elsewhere (`[machines.NAME] jobs_dir`).
        let root = machine
            .as_ref()
            .and_then(|machine| machine.jobs_dir.as_ref())
            .map_or_else(|| Arc::clone(root), |dir| Arc::from(dir.word()));
        Ok(Self {
            roster: Arc::clone(roster),
            machine,
            root,
        })
    }

    fn connection(&self) -> Connection<'_> {
        match &self.machine {
            Some(machine) => self.roster.connection(machine),
            None => Connection {
                machine: crate::name::local_name(),
                via: Via::Local,
            },
        }
    }

    /// The machine's login `PATH`, probing it first if this run has not.
    async fn facts(&self) -> Result<Option<Facts>, JobError> {
        match &self.machine {
            Some(machine) => Ok(Some(self.roster.facts_or_probe(machine).await?)),
            None => Ok(None),
        }
    }

    async fn short(&self, script: &str) -> Result<String, JobError> {
        Ok(self.connection().run_short(script, SHORT).await?)
    }
}

/// The value after `MARK` + `key` on the first line that has it.
fn reported<'a>(stdout: &'a str, key: &str) -> Option<&'a str> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(MARK)?.strip_prefix(key))
}

impl Supervisor for MachineSupervisor {
    fn start(&self, spec: JobSpec) -> BoxFuture<'_, Result<JobHandle, JobError>> {
        Box::pin(async move {
            let reach = Reach::new(&self.roster().await, &self.root, &spec.place)?;
            let facts = reach.facts().await?;
            let id = JobId::fresh();
            let cd = crate::script::cd_lines(&spec.start);
            let env: Vec<(String, String)> = spec
                .env
                .iter()
                .filter(|(key, _)| is_env_name(key))
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect();
            let launch = script::launch(
                &reach.root,
                id.as_str(),
                &spec.command,
                &cd,
                &env,
                facts.as_ref().and_then(|facts| facts.path.as_deref()),
            );
            let stdout = reach.short(&launch).await?;
            let launched = reported(&stdout, "job pid=").and_then(|rest| {
                let (pid, boot) = rest.split_once(" boot=")?;
                Some((pid.trim().parse::<i64>().ok()?, boot.trim().to_owned()))
            });
            let Some((pid, boot)) = launched else {
                return Err(JobError::Launch {
                    place: spec.place.clone(),
                    reason: reported(&stdout, "error ")
                        .unwrap_or("it did not report a process")
                        .to_owned(),
                });
            };
            let record = JobRecord {
                id,
                place: spec.place,
                label: spec.label,
                command: spec.command,
                pid,
                boot,
                started_at: crate::facts::now_seconds(),
            };
            Ok(follow(reach, record, Cursor::default()))
        })
    }

    fn adopt(&self, record: JobRecord) -> BoxFuture<'_, Result<JobHandle, JobError>> {
        Box::pin(async move {
            if record.is_in_process() {
                return Err(JobError::Lost {
                    reason: "it ran inside a Nori that has since exited".to_owned(),
                });
            }
            let reach = Reach::new(&self.roster().await, &self.root, &record.place)?;
            let check = script::check(&reach.root, record.id.as_str(), record.pid, &record.boot);
            let stdout = match reach.short(&check).await {
                Ok(stdout) => stdout,
                // The machine may be down for a while: follow anyway; the follower retries.
                Err(JobError::Machine(_)) => String::new(),
                Err(error) => return Err(error),
            };
            if reported(&stdout, "rebooted").is_some() {
                return Err(JobError::Lost {
                    reason: format!("{} restarted since the job started", record.place),
                });
            }
            // From the current end: what came before is read with `read` from the consumer's
            // own cursor.
            Ok(follow(
                reach,
                record,
                Cursor {
                    stdout: -1,
                    stderr: -1,
                },
            ))
        })
    }
}

/// Environment names a shell accepts; anything else is dropped rather than quoted into code.
fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Start following `record`'s output from `from`, and return its handle.
fn follow(reach: Reach, record: JobRecord, from: Cursor) -> JobHandle {
    let (lines, _) = broadcast::channel(LINE_BUFFER);
    let (state_tx, state) = watch::channel(JobState::Running { pid: record.pid });
    let ended_in = Arc::new(std::sync::Mutex::new(None));
    let control = Arc::new(Control {
        reach: reach.clone(),
        record: record.clone(),
        ended_in: Arc::clone(&ended_in),
    });
    let task = tokio::spawn(run_follower(
        reach,
        record.clone(),
        from,
        lines.clone(),
        state_tx,
        ended_in,
    ));
    JobHandle::new(record, lines, state, control, task.abort_handle())
}

/// Follow until the job ends, reconnecting with backoff while its machine does not answer.
async fn run_follower(
    reach: Reach,
    record: JobRecord,
    mut at: Cursor,
    lines: broadcast::Sender<JobLine>,
    state: watch::Sender<JobState>,
    ended_in: Arc<std::sync::Mutex<Option<String>>>,
) {
    let mut seq = 0;
    let mut backoff = Duration::from_millis(500);
    loop {
        let script = script::follow(
            &reach.root,
            record.id.as_str(),
            record.pid,
            at.stdout,
            at.stderr,
        );
        let outcome = match reach.connection().spawn_stream(&script).await {
            Ok(mut streaming) => {
                let stdout = streaming.child.stdout.take();
                let outcome = match stdout {
                    Some(stdout) => {
                        let mut reader = Framed::new(stdout, at);
                        let outcome = loop {
                            match reader.next().await {
                                Some(Frame::Line(stream, text)) => {
                                    seq += 1;
                                    let _ = lines.send(JobLine {
                                        stream,
                                        text,
                                        after: reader.at,
                                        seq,
                                    });
                                }
                                Some(Frame::End(end)) => break Some(end),
                                None => break None,
                            }
                        };
                        at = reader.at;
                        if let Some(cwd) = reader.cwd.take() {
                            *ended_in
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd);
                        }
                        outcome
                    }
                    None => None,
                };
                drop(streaming);
                outcome
            }
            Err(_) => None,
        };
        match outcome {
            Some(end) => {
                state.send_replace(end);
                return;
            }
            None => {
                state.send_replace(JobState::Unreachable {
                    reason: format!("{} does not answer", record.place),
                });
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

enum Frame {
    Line(Stream, String),
    End(JobState),
}

/// Reads the follower's framed output into whole lines, tracking the cursor.
struct Framed<R> {
    reader: BufReader<R>,
    at: Cursor,
    partial_out: Vec<u8>,
    partial_err: Vec<u8>,
    ready: std::collections::VecDeque<(Stream, String)>,
    ended: Option<JobState>,
    /// Where the command ended, once the follower said.
    cwd: Option<String>,
}

impl<R: AsyncRead + Unpin> Framed<R> {
    fn new(reader: R, at: Cursor) -> Self {
        Self {
            reader: BufReader::new(reader),
            at,
            partial_out: Vec::new(),
            partial_err: Vec::new(),
            ready: std::collections::VecDeque::new(),
            ended: None,
            cwd: None,
        }
    }

    async fn next(&mut self) -> Option<Frame> {
        loop {
            if let Some((stream, text)) = self.ready.pop_front() {
                return Some(Frame::Line(stream, text));
            }
            if let Some(end) = self.ended.take() {
                return Some(Frame::End(end));
            }
            let mut header = String::new();
            match self.reader.read_line(&mut header).await {
                Ok(0) | Err(_) => return None,
                Ok(_) => {}
            }
            let Some(report) = header.trim_end().strip_prefix(MARK) else {
                continue;
            };
            if let Some(rest) = report.strip_prefix("at ") {
                let mut parts = rest.split(' ').map(str::parse::<i64>);
                if let (Some(Ok(stdout)), Some(Ok(stderr))) = (parts.next(), parts.next()) {
                    self.at = Cursor { stdout, stderr };
                }
            } else if let Some(rest) = report.strip_prefix("chunk ") {
                let mut parts = rest.split(' ');
                let (Some(name), Some(_offset), Some(length)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    return None;
                };
                let Ok(length) = length.parse::<i64>() else {
                    return None;
                };
                let mut bytes = vec![0; usize::try_from(length).unwrap_or_default()];
                if self.reader.read_exact(&mut bytes).await.is_err() {
                    return None;
                }
                let stream = if name == "err" {
                    Stream::Stderr
                } else {
                    Stream::Stdout
                };
                self.take(stream, &bytes);
            } else if let Some(cwd) = report.strip_prefix("cwd ") {
                self.cwd = Some(cwd.to_owned());
            } else if let Some(exit) = report.strip_prefix("exit ") {
                self.flush_partials();
                self.ended = Some(JobState::from_exit_line(exit));
            } else if report == "gone" {
                self.flush_partials();
                self.ended = Some(JobState::Lost {
                    reason: "its process is gone and it recorded no end".to_owned(),
                });
            }
        }
    }

    fn take(&mut self, stream: Stream, bytes: &[u8]) {
        let (partial, offset) = match stream {
            Stream::Stdout => (&mut self.partial_out, &mut self.at.stdout),
            Stream::Stderr => (&mut self.partial_err, &mut self.at.stderr),
        };
        for &byte in bytes {
            if byte == b'\n' {
                let text = String::from_utf8_lossy(partial).into_owned();
                *offset += partial.len() as i64 + 1;
                partial.clear();
                self.ready.push_back((stream, text));
            } else {
                partial.push(byte);
            }
        }
    }

    /// A last line without a newline counts once the job has ended.
    fn flush_partials(&mut self) {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let (partial, offset) = match stream {
                Stream::Stdout => (&mut self.partial_out, &mut self.at.stdout),
                Stream::Stderr => (&mut self.partial_err, &mut self.at.stderr),
            };
            if !partial.is_empty() {
                *offset += partial.len() as i64;
                self.ready
                    .push_back((stream, String::from_utf8_lossy(partial).into_owned()));
                partial.clear();
            }
        }
    }
}

/// What a handle asks of a job on a real machine.
struct Control {
    reach: Reach,
    record: JobRecord,
    ended_in: Arc<std::sync::Mutex<Option<String>>>,
}

impl JobControl for Control {
    fn read(&self, from: ReadFrom, max_bytes: i64) -> BoxFuture<'_, Result<JobOutput, JobError>> {
        Box::pin(async move {
            let (start, skip_partial, keep_last) = match from {
                ReadFrom::Cursor(cursor) => (cursor, false, false),
                ReadFrom::Tail { bytes } => (
                    Cursor {
                        stdout: -bytes.max(1),
                        stderr: -bytes.max(1),
                    },
                    true,
                    false,
                ),
                ReadFrom::Everything => (Cursor::default(), false, true),
            };
            let script = script::read(
                &self.reach.root,
                self.record.id.as_str(),
                start.stdout,
                start.stderr,
                max_bytes,
            );
            let stdout = self
                .reach
                .connection()
                .run_short_bytes(&script, SHORT)
                .await?;
            Ok(parse_read(&stdout, start, skip_partial, keep_last))
        })
    }

    fn signal(&self, signal: Signal) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            self.reach
                .short(&script::signal(self.record.pid, signal.name()))
                .await
                .map(|_| ())
        })
    }

    fn final_cwd(&self) -> Option<String> {
        self.ended_in
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn forget(&self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            self.reach
                .short(&script::forget(&self.reach.root, self.record.id.as_str()))
                .await
                .map(|_| ())
        })
    }

    fn stop(&self, grace: Duration) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let tenths = (grace.as_millis() / 100) as i64;
            self.reach
                .short(&script::stop(
                    &self.reach.root,
                    self.record.id.as_str(),
                    self.record.pid,
                    tenths,
                ))
                .await
                .map(|_| ())
        })
    }
}

/// Lines of a framed read, each stream in order. A tail read starts mid-line, so it drops its
/// first piece; a last piece without a newline is left for the next read.
fn parse_read(bytes: &[u8], start: Cursor, skip_partial: bool, keep_last: bool) -> JobOutput {
    let mut output = JobOutput {
        lines: Vec::new(),
        next: Cursor {
            stdout: start.stdout.max(0),
            stderr: start.stderr.max(0),
        },
    };
    let mut rest = bytes;
    let mut seq = 0;
    while let Some(newline) = rest.iter().position(|&byte| byte == b'\n') {
        let header = String::from_utf8_lossy(&rest[..newline]).into_owned();
        rest = &rest[newline + 1..];
        let Some(chunk) = header
            .strip_prefix(MARK)
            .and_then(|report| report.strip_prefix("chunk "))
        else {
            continue;
        };
        let mut parts = chunk.split(' ');
        let (Some(name), Some(offset), Some(length)) = (parts.next(), parts.next(), parts.next())
        else {
            break;
        };
        let (Ok(mut offset), Ok(length)) = (offset.parse::<i64>(), length.parse::<usize>()) else {
            break;
        };
        let length = length.min(rest.len());
        let (body, after) = rest.split_at(length);
        rest = after;
        let stream = if name == "err" {
            Stream::Stderr
        } else {
            Stream::Stdout
        };
        let mut pieces: Vec<&[u8]> = body.split(|&byte| byte == b'\n').collect();
        // After the last newline: a partial line, read again next time, unless the job ended
        // and it is the last thing the job will ever write.
        let last = pieces.pop().filter(|last| keep_last && !last.is_empty());
        let count = pieces.len();
        for (index, piece) in pieces.into_iter().chain(last).enumerate() {
            offset += piece.len() as i64 + i64::from(index < count);
            match stream {
                Stream::Stdout => output.next.stdout = offset,
                Stream::Stderr => output.next.stderr = offset,
            }
            if skip_partial && index == 0 {
                continue;
            }
            seq += 1;
            output.lines.push(JobLine {
                stream,
                text: String::from_utf8_lossy(piece).into_owned(),
                after: output.next,
                seq,
            });
        }
    }
    output
}
