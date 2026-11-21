//! What the agent and the client read about jobs: the list, exact reads of output, stopping, and
//! the count the status line shows.

use std::time::Duration;

use nori_machines::Stream;
use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobState;
use nori_machines::jobs::ReadFrom;
use nori_machines::jobs::Signal;
use serde::Serialize;

use super::Entry;
use super::JobOrigin;
use super::JobSummary;
use super::JobWake;
use super::Jobs;
use super::ReadSpec;
use super::State;
use super::journal::Record;
use super::spec::place_name;
use super::spec::state_text;
use crate::backend::monitor::spec::Source;

/// A read returns at most this much of each stream, whatever the caller asks.
const MAX_READ_BYTES: i64 = 256 * 1024;
/// A read returns at least this much: a line longer than the limit could never be read.
const MIN_READ_BYTES: i64 = 8 * 1024;
/// A read that came this close to its limit probably left output behind.
const MORE_SLACK: i64 = 1024;
/// How long `stop` waits for the job to end after asking, beyond the grace it gave.
const STOP_PATIENCE: Duration = Duration::from_secs(3);
/// The longest a command is shown in the list.
const COMMAND_CHARS: usize = 200;

/// One job as `job_list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct JobView {
    pub(crate) job: String,
    pub(crate) label: String,
    pub(crate) command: String,
    pub(crate) place: String,
    pub(crate) pid: i64,
    pub(crate) state: String,
    pub(crate) origin: JobOrigin,
    pub(crate) wake: JobWake,
    pub(crate) running_seconds: i64,
}

/// Output of one read: whole lines, and where the next read starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct JobRead {
    pub(crate) job: String,
    pub(crate) label: String,
    pub(crate) state: String,
    /// Where the read started: `new`, `tail`, `start` or `cursor`.
    pub(crate) from: &'static str,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) stdout: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) stderr: String,
    /// Lines returned.
    pub(crate) lines: i64,
    /// Where the output stands after this read.
    pub(crate) cursor: Cursor,
    /// More output is probably waiting: read again.
    pub(crate) more: bool,
}

/// Jobs the session follows, counted for the status line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JobsState {
    pub(crate) running: i64,
    /// Running on a machine that does not answer right now.
    pub(crate) unreachable: i64,
}

impl Jobs {
    pub(crate) fn list(&self) -> Vec<JobView> {
        let now = chrono::Utc::now().timestamp();
        self.lock()
            .entries
            .iter()
            .map(|entry| JobView {
                job: entry.record.id.to_string(),
                label: entry.label.clone(),
                command: clip(&entry.record.command),
                place: place_name(&entry.record.place),
                pid: entry.record.pid,
                state: state_text(&entry.state),
                origin: entry.origin.clone(),
                wake: entry.wake.clone(),
                running_seconds: (now - entry.record.started_at).max(0),
            })
            .collect()
    }

    /// How many jobs are running, for the status line.
    pub(crate) fn summary(&self) -> JobsState {
        let state = self.lock();
        let count = |wanted: fn(&JobState) -> bool| {
            i64::try_from(
                state
                    .entries
                    .iter()
                    .filter(|entry| wanted(&entry.state))
                    .count(),
            )
            .unwrap_or(i64::MAX)
        };
        JobsState {
            running: count(|state| matches!(state, JobState::Running { .. })),
            unreachable: count(|state| matches!(state, JobState::Unreachable { .. })),
        }
    }

    /// Read a job's output. `New` returns exactly what the agent has not read and moves its
    /// cursor on; the other starts return what they say and never move it back.
    pub(crate) async fn read(
        &self,
        which: &str,
        from: ReadSpec,
        max_bytes: i64,
    ) -> Result<JobRead, String> {
        let max_bytes = max_bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES);
        let (id, label, state, cursor, handle) = {
            let state = self.lock();
            let entry = find(&state, which)?;
            let Some(handle) = entry.handle.clone() else {
                return Err(format!(
                    "`{}` ended in an earlier run ({}); its output is no longer followed",
                    entry.label,
                    state_text(&entry.state)
                ));
            };
            (
                entry.record.id.clone(),
                entry.label.clone(),
                state_text(&entry.state),
                entry.cursor,
                handle,
            )
        };
        let (read_from, name) = match from {
            ReadSpec::New => (ReadFrom::Cursor(cursor), "new"),
            ReadSpec::Start => (ReadFrom::Cursor(Cursor::default()), "start"),
            ReadSpec::Cursor(at) => (ReadFrom::Cursor(at), "cursor"),
            ReadSpec::Tail { bytes } => (
                ReadFrom::Tail {
                    bytes: bytes.clamp(MIN_READ_BYTES, MAX_READ_BYTES),
                },
                "tail",
            ),
        };
        let output = handle
            .read(read_from, max_bytes)
            .await
            .map_err(|error| error.to_string())?;
        let more = match read_from {
            ReadFrom::Cursor(at) => {
                let near = |before: i64, after: i64| after - before >= max_bytes - MORE_SLACK;
                near(at.stdout, output.next.stdout) || near(at.stderr, output.next.stderr)
            }
            ReadFrom::Tail { .. } | ReadFrom::Everything => false,
        };
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        for line in &output.lines {
            match line.stream {
                Stream::Stdout => stdout.push(line.text.as_str()),
                Stream::Stderr => stderr.push(line.text.as_str()),
            }
        }
        let advanced = Cursor {
            stdout: cursor.stdout.max(output.next.stdout),
            stderr: cursor.stderr.max(output.next.stderr),
        };
        if advanced != cursor {
            if let Some(entry) = self
                .lock()
                .entries
                .iter_mut()
                .find(|entry| entry.record.id == id)
            {
                entry.cursor = advanced;
            }
            self.write(&Record::Read {
                id: id.clone(),
                cursor: advanced,
            })
            .await;
        }
        Ok(JobRead {
            job: id.to_string(),
            label,
            state,
            from: name,
            stdout: stdout.join("\n"),
            stderr: stderr.join("\n"),
            lines: i64::try_from(output.lines.len()).unwrap_or(i64::MAX),
            cursor: advanced,
            more,
        })
    }

    /// Stop a running job: TERM to its process group and KILL after `grace` (no `signal`), or send
    /// `signal` and leave it to the job. The job's own wake is not fired by a stop the agent asked
    /// for. Returns how the job stands once it ends, or after a short wait.
    pub(crate) async fn stop(
        &self,
        which: &str,
        signal: Option<Signal>,
        grace: Duration,
    ) -> Result<JobSummary, String> {
        let (id, label, handle) = {
            let state = self.lock();
            let entry = find(&state, which)?;
            if !entry.running() {
                return Err(format!(
                    "`{}` is not running ({})",
                    entry.label,
                    state_text(&entry.state)
                ));
            }
            let Some(handle) = entry.handle.clone() else {
                return Err(format!("`{}` is not followed by this session", entry.label));
            };
            (entry.record.id.clone(), entry.label.clone(), handle)
        };
        let ends_it = matches!(signal, None | Some(Signal::Kill));
        if ends_it {
            // The agent asked for this end: it is not news. A monitor of someone else's with the
            // same label is left alone.
            let monitors = &self.shared.monitors;
            if let Ok((monitor, spec, true)) = monitors.find(&label)
                && matches!(&spec.source, Source::Job { job, .. } if *job == id.as_str())
            {
                let _ = monitors
                    .stop(&monitor, crate::backend::monitor::Ending::Stopped)
                    .await;
            }
        }
        match signal {
            Some(signal) => handle.signal(signal).await,
            None => handle.stop(grace).await,
        }
        .map_err(|error| error.to_string())?;
        if ends_it {
            handle.wait(grace + STOP_PATIENCE).await;
        }
        let state = self.lock();
        let entry = state
            .entries
            .iter()
            .find(|entry| entry.record.id == id)
            .ok_or_else(|| format!("`{label}` is gone"))?;
        let mut summary = entry.summary();
        // The watcher may not have seen the end yet: the handle's own state is the newer.
        summary.state = state_text(&handle.state().borrow());
        Ok(summary)
    }
}

/// The entry `which` names: a job id, else the running job with that label, else the newest ended
/// one.
fn find<'a>(state: &'a State, which: &str) -> Result<&'a Entry, String> {
    let which = which.trim();
    let hit = state
        .entries
        .iter()
        .find(|entry| entry.record.id.as_str() == which)
        .or_else(|| {
            state
                .entries
                .iter()
                .rev()
                .find(|entry| entry.label == which && entry.running())
        })
        .or_else(|| {
            state
                .entries
                .iter()
                .rev()
                .find(|entry| entry.label == which)
        });
    hit.ok_or_else(|| {
        if state.entries.is_empty() {
            format!("`{which}` names no job: you have not started any. `job_start` starts one.")
        } else {
            let existing: Vec<String> = state
                .entries
                .iter()
                .map(|entry| {
                    format!(
                        "{} `{}` ({})",
                        entry.record.id,
                        entry.label,
                        state_text(&entry.state)
                    )
                })
                .collect();
            format!(
                "`{which}` names no job of yours. You have: {}. Address one by id or label.",
                existing.join(", ")
            )
        }
    })
}

fn clip(command: &str) -> String {
    if command.chars().count() <= COMMAND_CHARS {
        return command.to_owned();
    }
    let mut clipped: String = command.chars().take(COMMAND_CHARS).collect();
    clipped.push('…');
    clipped
}
