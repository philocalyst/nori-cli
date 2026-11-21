//! A session's jobs in its conversation journal, so a resume finds them again.
//!
//! One small `job` command per change: a job started (with everything the supervisor needs to
//! adopt it: its record), a job ended, and how far the agent has read. Folding them in order says
//! which jobs were still running when the journal ended and where each read stood. Rewound
//! records are not visible to the fold, so a rewind gives back what it unsaid.

use nori_machines::jobs::Cursor;
use nori_machines::jobs::JobId;
use nori_machines::jobs::JobRecord;
use nori_machines::jobs::JobState;
use serde::Deserialize;
use serde::Serialize;

use super::spec::JobOrigin;
use super::spec::JobWake;

/// The command name of every record.
pub(crate) const COMMAND: &str = "job";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Record {
    Started {
        record: JobRecord,
        origin: JobOrigin,
        wake: JobWake,
    },
    Ended {
        id: JobId,
        state: JobState,
    },
    /// The agent has read everything before `cursor`.
    Read {
        id: JobId,
        cursor: Cursor,
    },
}

/// One job as the journal left it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RestoredJob {
    pub(crate) record: JobRecord,
    pub(crate) origin: JobOrigin,
    pub(crate) wake: JobWake,
    pub(crate) cursor: Cursor,
    /// How it ended; `None` means it was still running when the journal ended.
    pub(crate) ended: Option<JobState>,
}

/// Ended jobs kept in a restored list.
const KEEP_ENDED: usize = 16;

/// The jobs `records` started, oldest first, with how they ended and how far each was read.
pub(crate) fn fold(records: impl IntoIterator<Item = Record>) -> Vec<RestoredJob> {
    let mut jobs: Vec<RestoredJob> = Vec::new();
    for record in records {
        match record {
            Record::Started {
                record,
                origin,
                wake,
            } => match jobs.iter_mut().find(|job| job.record.id == record.id) {
                Some(job) => {
                    job.record = record;
                    job.origin = origin;
                    job.wake = wake;
                    job.ended = None;
                }
                None => jobs.push(RestoredJob {
                    record,
                    origin,
                    wake,
                    cursor: Cursor::default(),
                    ended: None,
                }),
            },
            Record::Ended { id, state } => {
                if let Some(job) = jobs.iter_mut().find(|job| job.record.id == id) {
                    job.ended = Some(state);
                }
            }
            Record::Read { id, cursor } => {
                if let Some(job) = jobs.iter_mut().find(|job| job.record.id == id) {
                    job.cursor = Cursor {
                        stdout: job.cursor.stdout.max(cursor.stdout),
                        stderr: job.cursor.stderr.max(cursor.stderr),
                    };
                }
            }
        }
    }
    let ended = jobs.iter().filter(|job| job.ended.is_some()).count();
    let mut surplus = ended.saturating_sub(KEEP_ENDED);
    jobs.retain(|job| {
        if job.ended.is_some() && surplus > 0 {
            surplus -= 1;
            false
        } else {
            true
        }
    });
    jobs
}

#[cfg(test)]
mod tests {
    use nori_machines::jobs::Place;
    use pretty_assertions::assert_eq;

    use super::*;

    fn record(id: &str, label: &str) -> JobRecord {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "place": Place::Local,
            "label": label,
            "command": "sleep 60",
            "pid": 4242,
            "boot": "boot-1",
            "started_at": 1_700_000_000,
        }))
        .unwrap_or_else(|error| unreachable!("a job record: {error}"))
    }

    fn started(id: &str, label: &str) -> Record {
        Record::Started {
            record: record(id, label),
            origin: JobOrigin::Agent,
            wake: JobWake::default(),
        }
    }

    fn id(name: &str) -> JobId {
        serde_json::from_value(serde_json::json!(name))
            .unwrap_or_else(|error| unreachable!("a job id: {error}"))
    }

    #[test]
    fn a_job_started_and_not_ended_comes_back_with_how_far_it_was_read() {
        let restored = fold([
            started("j1", "dev"),
            started("j2", "tests"),
            Record::Read {
                id: id("j1"),
                cursor: Cursor {
                    stdout: 40,
                    stderr: 7,
                },
            },
            Record::Ended {
                id: id("j2"),
                state: JobState::Exited { code: 0 },
            },
            // A read can only move forward.
            Record::Read {
                id: id("j1"),
                cursor: Cursor {
                    stdout: 10,
                    stderr: 9,
                },
            },
        ]);
        assert_eq!(
            restored,
            vec![
                RestoredJob {
                    record: record("j1", "dev"),
                    origin: JobOrigin::Agent,
                    wake: JobWake::default(),
                    cursor: Cursor {
                        stdout: 40,
                        stderr: 9,
                    },
                    ended: None,
                },
                RestoredJob {
                    record: record("j2", "tests"),
                    origin: JobOrigin::Agent,
                    wake: JobWake::default(),
                    cursor: Cursor::default(),
                    ended: Some(JobState::Exited { code: 0 }),
                },
            ]
        );
    }

    #[test]
    fn a_job_started_again_under_the_same_id_is_running_again() {
        let restored = fold([
            started("j1", "dev"),
            Record::Ended {
                id: id("j1"),
                state: JobState::Killed {
                    signal: "TERM".to_owned(),
                },
            },
            started("j1", "dev"),
        ]);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].ended, None);
    }

    #[test]
    fn only_the_newest_ended_jobs_are_kept() {
        let mut records = Vec::new();
        for index in 0..20 {
            let name = format!("j{index}");
            records.push(started(&name, &name));
            records.push(Record::Ended {
                id: id(&name),
                state: JobState::Exited { code: 0 },
            });
        }
        records.push(started("running", "running"));
        let restored = fold(records);
        assert_eq!(restored.len(), KEEP_ENDED + 1);
        assert_eq!(restored[0].record.label, "j4");
        assert_eq!(
            restored.last().map(|job| job.record.label.as_str()),
            Some("running")
        );
    }
}
