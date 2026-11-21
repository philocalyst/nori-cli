//! Taking a journal's jobs back: each one that was running is adopted by the supervisor again; a
//! job that cannot be found is lost, and the agent and the person are told.

use std::collections::HashMap;

use nori_machines::jobs::JobId;
use nori_machines::jobs::JobRecord;
use nori_machines::jobs::JobState;
use nori_machines::jobs::Place;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;

use super::Entry;
use super::Followed;
use super::Jobs;
use super::journal;
use super::journal::Record;
use super::journal::RestoredJob;
use super::spec::JobOrigin;
use super::spec::JobWake;
use super::trim_ended;
use crate::backend::AcpBackend;
use crate::backend::BackendEvent;

/// A job that was running and could not be adopted.
struct Lost {
    label: String,
    reason: String,
}

/// A job the journal left running, listed and waiting to be found again.
struct Pending {
    record: JobRecord,
    origin: JobOrigin,
    wake: JobWake,
    /// Its record named the computer that wrote it, before the session moved here: the record
    /// with the machine it runs on is journaled before it is adopted.
    relocated: bool,
    /// Known before asking any supervisor: it runs on a machine this one cannot reach.
    unreachable: Option<String>,
}

impl Jobs {
    /// Take what a journal says, at once and without asking any machine: every job is listed, an
    /// ended one as it ended, a running one as being found again (`unreachable` until it is).
    /// Returns the ones to adopt.
    /// `placed` says where the jobs a moved session left running are, seen from here (see
    /// [`super::moves`]).
    fn take(
        &self,
        restored: Vec<RestoredJob>,
        placed: &HashMap<JobId, Result<Place, String>>,
    ) -> Vec<Pending> {
        let mut pending = Vec::new();
        let mut state = self.lock();
        for job in restored {
            let RestoredJob {
                mut record,
                origin,
                wake,
                cursor,
                ended,
            } = job;
            let running = ended.is_none();
            let (relocated, unreachable) = match placed.get(&record.id).filter(|_| running) {
                Some(Ok(place)) if *place != record.place => {
                    record.place = place.clone();
                    (true, None)
                }
                Some(Ok(_)) | None => (false, None),
                Some(Err(reason)) => (false, Some(reason.clone())),
            };
            state.entries.push(Entry {
                label: record.label.clone(),
                record: record.clone(),
                origin: origin.clone(),
                wake: wake.clone(),
                handle: None,
                state: ended.unwrap_or_else(|| JobState::Unreachable {
                    reason: "being found again after the restart".to_owned(),
                }),
                cursor,
                watcher: None,
            });
            if running {
                pending.push(Pending {
                    record,
                    origin,
                    wake,
                    relocated,
                    unreachable,
                });
            }
        }
        trim_ended(&mut state);
        pending
    }

    /// Ask the supervisor for one job the journal left running: followed again with its wake, or
    /// lost (journaled, and its wake says so).
    async fn readopt(
        &self,
        Pending {
            record,
            origin,
            wake,
            relocated,
            unreachable,
        }: Pending,
    ) -> Option<Lost> {
        let id = record.id.clone();
        let label = record.label.clone();
        if relocated {
            self.write(&Record::Started {
                record: record.clone(),
                origin: origin.clone(),
                wake: wake.clone(),
            })
            .await;
        }
        let adopted = match unreachable {
            Some(reason) => Err(reason),
            None => self
                .supervisor()
                .adopt(record)
                .await
                .map_err(|error| error.to_string()),
        };
        let (followed, lost) = match adopted {
            Ok(handle) => {
                let followed = Followed::live(&handle);
                if let Some(entry) = self
                    .lock()
                    .entries
                    .iter_mut()
                    .find(|entry| entry.record.id == id)
                {
                    entry.state = handle.state().borrow().clone();
                    entry.handle = Some(handle.clone());
                }
                self.watch(&id, &handle);
                (followed, None)
            }
            Err(reason) => {
                let state = JobState::Lost {
                    reason: reason.clone(),
                };
                if let Some(entry) = self
                    .lock()
                    .entries
                    .iter_mut()
                    .find(|entry| entry.record.id == id)
                {
                    entry.state = state.clone();
                }
                self.write(&Record::Ended {
                    id: id.clone(),
                    state: state.clone(),
                })
                .await;
                (
                    Followed::Over(state),
                    Some(Lost {
                        label: label.clone(),
                        reason,
                    }),
                )
            }
        };
        if let Err(error) = self.rearm_wake(&id, &label, &wake, followed).await {
            tracing::warn!(%error, job = %id, "a resumed job's wake was not armed");
        }
        // Watches its modules declare, if they loaded first; else `declare_watches` arms them.
        if lost.is_none() {
            for problem in self.arm_declared(&id, &label, &origin).await {
                tracing::warn!(job = %id, %problem, "a declared job watch was not armed");
            }
        }
        self.shared.monitors.notify();
        lost
    }
}

impl AcpBackend {
    /// Adopt the jobs the journal left running, after the monitors are back: a job's wake is the
    /// dormant monitor that restoring them brought back.
    pub(in crate::backend) async fn restore_jobs(&self) {
        let Some(recorder) = self.transcript_recorder.read().await.clone() else {
            return;
        };
        let commands = match recorder
            .journaled_commands_of(&[journal::COMMAND, crate::portable::MOVED_IN])
            .await
        {
            Ok(commands) => commands,
            Err(error) => {
                tracing::warn!(%error, "failed to read the journaled jobs");
                return;
            }
        };
        let (records, elsewhere) = super::moves::sort_out(commands);
        if records.is_empty() {
            return;
        }
        let jobs = self.orchestrator.jobs().clone();
        jobs.attach_journal(self.transcript_recorder_cell());
        let placed: HashMap<JobId, Result<Place, String>> = if elsewhere.is_empty() {
            HashMap::new()
        } else {
            let roster = nori_machines::global_ready().await;
            elsewhere
                .into_iter()
                .map(|(id, moved)| {
                    let place = moved.place(&roster);
                    (id, place)
                })
                .collect()
        };
        let pending = jobs.take(journal::fold(records), &placed);
        jobs.shared.monitors.notify();
        let events = self.backend_event_tx.clone();
        // A machine may take seconds to answer, or not answer: the session starts now, and the
        // jobs are found again side by side behind it. The runtime may not be draining events yet
        // either: nothing here waits for it.
        tokio::spawn(async move {
            let lost =
                futures::future::join_all(pending.into_iter().map(|job| jobs.readopt(job))).await;
            for job in lost.into_iter().flatten() {
                let message = format!("⚙ job {} is lost: {}", job.label, job.reason);
                let _ = events
                    .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                        nori_protocol::Notice { message },
                    ))))
                    .await;
            }
        });
    }
}
