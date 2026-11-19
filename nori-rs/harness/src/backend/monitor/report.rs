//! What a running monitor reports: events queued in order, journaled before anyone can see them,
//! and the end of a run.

use std::time::Instant;

use super::event::Class;
use super::event::Ending;
use super::event::Event;
use super::event::EventLine;
use super::journal::Record;
use super::registry::Burst;
use super::registry::Entry;
use super::registry::Monitors;
use super::registry::Phase;
use super::registry::Queued;
use super::registry::seconds;
use super::spec::WakeMode;

impl Monitors {
    // ---- what a running monitor reports ----------------------------------------------------

    /// When the monitor of `run` expires, or `None` when that run is over.
    pub(super) fn deadline(&self, id: &str, run: i64) -> Option<Instant> {
        match self.lock().entry(id) {
            Some(Entry {
                run: current,
                phase: Phase::Running { deadline },
                ..
            }) if *current == run => Some(*deadline),
            Some(_) | None => None,
        }
    }

    /// Queue a burst of lines as an event. Returns `false` when the run is over: its task must
    /// stop.
    pub(super) async fn emit(&self, id: &str, run: i64, burst: Burst) -> bool {
        let Some(event) = self.build(id, run, Some(burst), None) else {
            return false;
        };
        self.enqueue(id, run, event, true).await
    }

    /// End the run of a monitor, and tell the agent unless it asked for the end.
    pub(super) async fn finish(&self, id: &str, run: i64, burst: Option<Burst>, ending: Ending) {
        let event = {
            // What the source said last and how it ended are one event: nobody is woken twice
            // for the same moment.
            let event = if ending.is_reported() || burst.is_some() {
                self.build(id, run, burst, Some(ending.clone()))
            } else {
                None
            };
            let mut state = self.lock();
            let Some(entry) = state.entry(id) else { return };
            if entry.run != run || !matches!(entry.phase, Phase::Running { .. }) {
                return;
            }
            entry.task = None;
            entry.phase = Phase::Ended(ending.clone());
            event
        };
        if let Some(event) = event {
            self.enqueue(id, run, event, false).await;
        }
        self.write(&Record::Ended {
            id: id.to_owned(),
            ending,
        })
        .await;
        self.notify();
    }

    /// The event for `burst` and/or `ending`, with its place in the order, or `None` when the run
    /// is over.
    fn build(
        &self,
        id: &str,
        run: i64,
        burst: Option<Burst>,
        ending: Option<Ending>,
    ) -> Option<Event> {
        let mut state = self.lock();
        state.next_seq += 1;
        let seq = state.next_seq;
        let entry = state.entry(id)?;
        if entry.run != run || !matches!(entry.phase, Phase::Running { .. }) {
            return None;
        }
        let age = i64::try_from(entry.started.elapsed().as_secs()).unwrap_or(i64::MAX);
        let (mut lines, omitted) =
            burst.map_or((Vec::new(), 0), |burst| (burst.lines, burst.omitted));
        if !lines.is_empty() {
            entry.emitted += 1;
        }
        if let Some(ending) = ending.as_ref().filter(|ending| ending.is_reported()) {
            lines.push(EventLine {
                text: ending.sentence(entry.emitted, entry.failures),
                failure: ending.is_failure(),
            });
        }
        let failure = lines.iter().any(|line| line.failure);
        entry.failures += i64::from(failure);
        entry.last = lines
            .iter()
            .find(|line| line.failure)
            .or_else(|| lines.last())
            .map(|line| line.text.clone());
        let class = if failure {
            Class::Urgent
        } else {
            match entry.spec.wake {
                WakeMode::Immediate => Class::Immediate,
                WakeMode::Digest => Class::Digest,
                WakeMode::Boundary => Class::Boundary,
            }
        };
        Some(Event {
            seq,
            monitor: entry.id.clone(),
            label: entry.spec.label.clone(),
            age_seconds: age,
            lines,
            omitted,
            failure,
            ending,
            class,
        })
    }

    /// Journal an event, then queue it. An event is never queued before it is journaled.
    async fn enqueue(&self, id: &str, run: i64, event: Event, must_be_running: bool) -> bool {
        self.write(&Record::Event {
            event: event.clone(),
        })
        .await;
        let digest = {
            let state = self.lock();
            state
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .map_or(30, |entry| entry.spec.limits.digest_seconds)
        };
        let stale = {
            let mut state = self.lock();
            let current = state
                .entries
                .iter()
                .find(|entry| entry.id == id)
                .is_some_and(|entry| {
                    entry.run == run
                        && (!must_be_running || matches!(entry.phase, Phase::Running { .. }))
                });
            if current {
                let now = Instant::now();
                let ready = match event.class {
                    Class::Urgent | Class::Immediate => Some(now),
                    Class::Digest => Some(now + seconds(digest)),
                    Class::Boundary => None,
                };
                state.insert(Queued {
                    event: event.clone(),
                    at: now,
                    ready,
                });
            }
            !current
        };
        if stale {
            self.write(&Record::Taken {
                seqs: vec![event.seq],
            })
            .await;
            return false;
        }
        self.notify();
        true
    }
}
