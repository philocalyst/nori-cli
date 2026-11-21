//! What the agent can read about its monitors: the list, the unread events, the summary a client
//! shows.

use serde::Serialize;

use super::event::Event;
use super::journal::Record;
use super::registry::Monitors;
use super::registry::Phase;
use super::registry::State;
use std::time::Instant;

/// One monitor as `monitor_list` shows it.
#[derive(Debug, Serialize)]
pub(crate) struct View {
    pub(crate) monitor: String,
    pub(crate) label: String,
    pub(crate) source: &'static str,
    pub(crate) watching: String,
    pub(crate) state: String,
    pub(crate) wake: &'static str,
    pub(crate) events: i64,
    pub(crate) failures: i64,
    pub(crate) unread: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last: Option<String>,
    pub(crate) running_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expires_in_seconds: Option<i64>,
}

/// What stopping a monitor left behind.
#[derive(Debug, Serialize)]
pub(crate) struct Stopped {
    pub(crate) monitor: String,
    pub(crate) label: String,
    pub(crate) events: i64,
    pub(crate) failures: i64,
}

impl Monitors {
    // ---- reading ---------------------------------------------------------------------------

    pub(crate) fn list(&self) -> Vec<View> {
        let state = self.lock();
        state
            .entries
            .iter()
            .map(|entry| {
                let unread = state
                    .queue
                    .iter()
                    .filter(|queued| queued.event.monitor == entry.id)
                    .count();
                View {
                    monitor: entry.id.clone(),
                    label: entry.spec.label.clone(),
                    source: entry.spec.source.kind(),
                    watching: entry.spec.source.describe(),
                    state: State::status_of(entry),
                    wake: entry.spec.wake.label(),
                    events: entry.emitted,
                    failures: entry.failures,
                    unread: i64::try_from(unread).unwrap_or(i64::MAX),
                    last: entry.last.clone(),
                    running_seconds: i64::try_from(entry.started.elapsed().as_secs())
                        .unwrap_or(i64::MAX),
                    expires_in_seconds: match entry.phase {
                        Phase::Running { deadline } => Some(
                            i64::try_from(
                                deadline.saturating_duration_since(Instant::now()).as_secs(),
                            )
                            .unwrap_or(i64::MAX),
                        ),
                        Phase::Dormant | Phase::Ended(_) => None,
                    },
                }
            })
            .collect()
    }

    /// Hand over every queued event that no batch holds: a tool result carries them, so none is
    /// delivered twice.
    pub(crate) async fn take_unread(&self) -> Vec<Event> {
        let events: Vec<Event> = {
            let mut state = self.lock();
            state.dropped = 0;
            state.queue.drain(..).map(|queued| queued.event).collect()
        };
        if !events.is_empty() {
            self.write(&Record::Taken {
                seqs: events.iter().map(|event| event.seq).collect(),
            })
            .await;
        }
        events
    }

    /// The armed monitors as a client shows them.
    pub(crate) fn summary(&self) -> nori_protocol::MonitorsState {
        let state = self.lock();
        let labels: Vec<String> = state
            .running()
            .map(|entry| entry.spec.label.clone())
            .collect();
        nori_protocol::MonitorsState {
            watching: i64::try_from(labels.len()).unwrap_or(i64::MAX),
            dormant: i64::try_from(
                state
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry.phase, Phase::Dormant))
                    .count(),
            )
            .unwrap_or(i64::MAX),
            labels,
            // The jobs are counted by the backend, which owns them.
            jobs: 0,
        }
    }
}

/// Said when an address names no monitor of the actor's: what exists.
pub(super) fn unknown(state: &State, which: &str) -> String {
    let existing: Vec<String> = state
        .entries
        .iter()
        .map(|entry| {
            format!(
                "{} `{}` ({})",
                entry.id,
                entry.spec.label,
                State::status_of(entry)
            )
        })
        .collect();
    if existing.is_empty() {
        format!("`{which}` names no monitor: you have not armed any. `monitor_start` arms one.")
    } else {
        format!(
            "`{which}` names no monitor of yours. You have: {}. Address one by id or label.",
            existing.join(", ")
        )
    }
}
