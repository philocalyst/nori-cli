//! The `job` source: a background job's output lines, the moment it is ready, and how it ends.
//!
//! It follows the job's [`JobHandle`](nori_machines::jobs::JobHandle) and sends what the monitor's
//! rules then filter. Lines the source says itself (ready, unreachable) start with `job `, so a
//! `filter` written for the job's own output never hides them ([`NOTE_PATTERN`]).

use nori_machines::Stream;
use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobHandle;
use nori_machines::jobs::JobLine;
use nori_machines::jobs::JobState;
use nori_machines::jobs::ReadFrom;
use regex::Regex;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::jobs::Followed;
use crate::backend::monitor::event::Ending;

/// What the lines this source says itself begin with: `job ready: ...`, `job unreachable: ...`.
pub(crate) const NOTE_PATTERN: &str = "^job (?:ready|unreachable|reachable)";
/// How much of the job's output a lagging follower reads to catch up.
const CATCH_UP_BYTES: i64 = 1 << 20;
/// How much of a failed job's stderr is searched for its last words, and how many lines of it
/// the failure carries.
const LAST_WORDS_BYTES: i64 = 4096;
const LAST_WORDS_LINES: usize = 5;

/// A job and what to say about it, resolved when the monitor was armed.
pub(crate) struct Prepared {
    pub(super) followed: Followed,
    pub(super) ready: Option<Regex>,
}

impl Prepared {
    /// The pattern is checked now, so a mistake is the arming call's error.
    pub(crate) fn new(followed: Followed, ready: Option<&str>) -> Result<Self, String> {
        let ready = ready
            .map(|pattern| {
                Regex::new(pattern).map_err(|error| {
                    format!("`ready` is not a valid regular expression ({pattern:?}): {error}")
                })
            })
            .transpose()?;
        Ok(Self { followed, ready })
    }
}

/// What a job's last state means to a monitor.
fn ending(state: &JobState) -> Ending {
    match state {
        JobState::Exited { code } => Ending::Exited { code: *code },
        JobState::Killed { signal } => Ending::Finished {
            ok: false,
            note: format!("The job was killed by signal {signal}."),
        },
        JobState::Lost { reason } => Ending::Failed {
            reason: format!("the job is lost: {reason}"),
        },
        JobState::Running { .. } | JobState::Unreachable { .. } => Ending::Failed {
            reason: "the job is still running, but nothing follows it any more".to_owned(),
        },
    }
}

/// What one run of the source keeps between lines.
struct Follow<'a> {
    lines: bool,
    ready: Option<&'a Regex>,
    ready_sent: bool,
    /// Where the last line sent ended: a line at or before it was already sent.
    seen: Cursor,
}

impl Follow<'_> {
    /// Send `line` as the monitor should hear it. Returns `false` when nobody listens any more.
    async fn forward(&mut self, line: JobLine, tx: &Sender<Item>) -> bool {
        let (seen, after) = match line.stream {
            Stream::Stdout => (&mut self.seen.stdout, line.after.stdout),
            Stream::Stderr => (&mut self.seen.stderr, line.after.stderr),
        };
        if after <= *seen {
            return true;
        }
        *seen = after;
        let is_ready =
            !self.ready_sent && self.ready.is_some_and(|ready| ready.is_match(&line.text));
        let text = if is_ready {
            self.ready_sent = true;
            format!("job ready: {}", line.text)
        } else if self.lines {
            line.text
        } else {
            return true;
        };
        tx.send(Item::Line {
            text,
            failure: false,
        })
        .await
        .is_ok()
    }
}

/// Send what the job printed after the last line sent, from its log. A receiver that lagged, or
/// that is about to stop at the job's end, has missed lines the log still has. Returns `false`
/// when nobody listens any more.
async fn catch_up(handle: &JobHandle, follow: &mut Follow<'_>, tx: &Sender<Item>) -> bool {
    loop {
        let before = follow.seen;
        let Ok(output) = handle.read(ReadFrom::Cursor(before), CATCH_UP_BYTES).await else {
            return true;
        };
        for line in output.lines {
            if !follow.forward(line, tx).await {
                return false;
            }
        }
        if follow.seen == before {
            return true;
        }
    }
}

pub(super) async fn run(
    prepared: Prepared,
    lines_wanted: bool,
    tx: &Sender<Item>,
) -> Option<Ending> {
    let Prepared { followed, ready } = prepared;
    let (handle, lines, mut state) = match followed {
        Followed::Live {
            handle,
            lines,
            state,
        } => (handle, lines, state),
        Followed::Over(state) => return Some(ending(&state)),
    };
    let mut lines = Some(lines);
    let mut follow = Follow {
        lines: lines_wanted,
        ready: ready.as_ref(),
        ready_sent: false,
        seen: Cursor::default(),
    };
    let mut reachable = true;
    loop {
        let current = state.borrow_and_update().clone();
        match &current {
            JobState::Running { .. } => {
                if !reachable {
                    reachable = true;
                    let note = Item::Line {
                        text: "job reachable: its machine answers again".to_owned(),
                        failure: false,
                    };
                    if tx.send(note).await.is_err() {
                        return None;
                    }
                }
            }
            JobState::Unreachable { reason } => {
                if reachable {
                    reachable = false;
                    let note = Item::Line {
                        text: format!("job unreachable: {reason}"),
                        failure: false,
                    };
                    if tx.send(note).await.is_err() {
                        return None;
                    }
                }
            }
            JobState::Exited { .. } | JobState::Killed { .. } | JobState::Lost { .. } => {
                // The job's log has every line it printed: a receiver that was lagging would have
                // missed some of them in the channel.
                if !catch_up(&handle, &mut follow, tx).await {
                    return None;
                }
                // A job that failed says why on its stderr: the failure carries what it said last,
                // so the agent need not read the job to learn what went wrong.
                if matches!(current, JobState::Exited { code } if code != 0)
                    && let Ok(tail) = handle
                        .read(
                            ReadFrom::Tail {
                                bytes: LAST_WORDS_BYTES,
                            },
                            LAST_WORDS_BYTES,
                        )
                        .await
                {
                    let said: Vec<String> = tail
                        .lines
                        .into_iter()
                        // A line the monitor already sent (it asked for output) is not sent twice.
                        .filter(|line| {
                            line.stream == Stream::Stderr
                                && !(follow.lines && line.after.stderr <= follow.seen.stderr)
                        })
                        .map(|line| line.text)
                        .collect();
                    let first = said.len().saturating_sub(LAST_WORDS_LINES);
                    for text in said.into_iter().skip(first) {
                        let last_words = Item::Line {
                            text,
                            failure: true,
                        };
                        if tx.send(last_words).await.is_err() {
                            return None;
                        }
                    }
                }
                return Some(ending(&current));
            }
        }
        let next_line = async {
            match lines.as_mut() {
                Some(lines) => lines.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            received = next_line => match received {
                Ok(line) => {
                    if !follow.forward(line, tx).await {
                        return None;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Lines were missed: what the job wrote since the last one sent is in its files.
                    if !catch_up(&handle, &mut follow, tx).await {
                        return None;
                    }
                }
                Err(RecvError::Closed) => lines = None,
            },
            changed = state.changed() => {
                if changed.is_err() {
                    return Some(Ending::Failed {
                        reason: "the supervisor stopped following the job".to_owned(),
                    });
                }
            }
        }
    }
}
