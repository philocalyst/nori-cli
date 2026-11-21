//! Delivery: which queued events are ready to wake an idle agent, and the lease that makes a batch
//! of them delivered exactly once.
//!
//! A lease takes every queued event out of the queue under the lock. The batch is then offered
//! to the turn ledger. If a turn starts with it, the lease is committed; if the ledger refuses,
//! the events go back in their places. Nothing else can take them in between.

use std::time::Duration;
use std::time::Instant;

use super::event::Class;
use super::event::Event;
use super::registry::Monitors;
use super::registry::Phase;
use super::registry::Queued;
use super::registry::State;

/// This many wakes within [`STORM_WINDOW`] make a storm: events that would each have woken the
/// agent are gathered into a digest instead.
const STORM_WAKES: usize = 6;
const STORM_WINDOW: Duration = Duration::from_secs(60);
const STORM_DIGEST: Duration = Duration::from_secs(30);

/// A batch of events being offered to the agent.
pub(crate) struct Lease {
    pub(crate) batch: String,
    pub(crate) events: Vec<Event>,
    pub(crate) dropped: i64,
    pub(crate) monitors: Vec<LeasedMonitor>,
}

/// A monitor that has events in a lease, as it stands.
pub(crate) struct LeasedMonitor {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) source: &'static str,
    pub(crate) status: String,
    pub(crate) rearmable: bool,
    /// What the monitor's `wake` action says ahead of its events.
    pub(crate) message: Option<String>,
}

impl Monitors {
    // ---- delivery --------------------------------------------------------------------------

    /// A turn ended. Returns whether it was an ordinary turn (not one that delivered events), so
    /// events that wait for a boundary may come now.
    pub(crate) fn turn_boundary(&self) -> bool {
        !std::mem::take(&mut self.lock().monitor_turn)
    }

    /// Take the queued events for delivery, if any is ready to wake an agent that is idle now.
    /// `at_boundary` is whether a turn has just ended.
    pub(crate) fn lease(&self, at_boundary: bool) -> Option<Lease> {
        let mut state = self.lock();
        if state.closed || state.leased || state.queue.is_empty() {
            return None;
        }
        let now = Instant::now();
        while state
            .wakes
            .front()
            .is_some_and(|woke| now.duration_since(*woke) > STORM_WINDOW)
        {
            state.wakes.pop_front();
        }
        let storm = state.wakes.len() >= STORM_WAKES;
        let due = state.queue.iter().any(|queued| {
            (queued.event.class == Class::Boundary && at_boundary) || is_ready(queued, now, storm)
        });
        if !due {
            return None;
        }
        let events: Vec<Event> = state.queue.drain(..).map(|queued| queued.event).collect();
        let dropped = std::mem::take(&mut state.dropped);
        let mut monitors: Vec<LeasedMonitor> = Vec::new();
        for event in &events {
            if monitors.iter().any(|monitor| monitor.id == event.monitor) {
                continue;
            }
            if let Some(entry) = state.entries.iter().find(|entry| entry.id == event.monitor) {
                monitors.push(LeasedMonitor {
                    id: entry.id.clone(),
                    label: entry.spec.label.clone(),
                    source: entry.spec.source.kind(),
                    status: State::status_of(entry),
                    rearmable: !matches!(entry.phase, Phase::Running { .. }),
                    message: entry.spec.wake_message().map(str::to_owned),
                });
            }
        }
        let lease = Lease {
            batch: format!("b-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]),
            events,
            dropped,
            monitors,
        };
        state.leased = true;
        Some(lease)
    }

    /// The batch reached the agent.
    pub(crate) fn commit(&self) {
        let mut state = self.lock();
        state.leased = false;
        state.monitor_turn = true;
        state.wakes.push_back(Instant::now());
    }

    /// The batch did not start a turn: its events wait for the next chance, in their place.
    pub(crate) fn release(&self, lease: Lease) {
        let mut state = self.lock();
        state.leased = false;
        state.dropped += lease.dropped;
        let now = Instant::now();
        for event in lease.events {
            let ready = match event.class {
                Class::Urgent | Class::Immediate => Some(now),
                Class::Digest => Some(now),
                Class::Boundary => None,
            };
            state.insert(Queued {
                event,
                at: now,
                ready,
            });
        }
    }

    /// When the delivery loop next needs to look at the queue by itself: the earliest event that
    /// becomes ready later (a digest filling up, or a storm's wait ending).
    pub(crate) fn next_ready(&self) -> Option<Instant> {
        let state = self.lock();
        let now = Instant::now();
        let storm = state
            .wakes
            .iter()
            .filter(|woke| now.duration_since(**woke) <= STORM_WINDOW)
            .count()
            >= STORM_WAKES;
        state
            .queue
            .iter()
            .filter_map(|queued| match (queued.event.class, queued.ready) {
                (Class::Immediate, Some(_)) if storm => Some(queued.at + STORM_DIGEST),
                (Class::Immediate | Class::Digest, Some(ready)) => Some(ready),
                (Class::Urgent | Class::Immediate | Class::Digest | Class::Boundary, _) => None,
            })
            .filter(|ready| *ready > now)
            .min()
    }
}

fn is_ready(queued: &Queued, now: Instant, storm: bool) -> bool {
    match (queued.event.class, queued.ready) {
        (Class::Urgent, _) => true,
        (Class::Immediate, Some(_)) if storm => now >= queued.at + STORM_DIGEST,
        (Class::Immediate | Class::Digest, Some(ready)) => now >= ready,
        (Class::Immediate | Class::Digest | Class::Boundary, None) | (Class::Boundary, Some(_)) => {
            false
        }
    }
}
