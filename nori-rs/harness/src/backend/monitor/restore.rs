//! Taking a journal's monitors back: armed ones come back dormant, unseen events queued again.

use std::sync::Arc;
use std::time::Instant;

use super::event::Class;
use super::event::Event;
use super::event::EventLine;
use super::journal::Restored;
use super::registry::Entry;
use super::registry::Monitors;
use super::registry::Phase;
use super::registry::Queued;
use super::spec::Source;

impl Monitors {
    // ---- restoring -------------------------------------------------------------------------

    /// Take what a journal says: its armed monitors come back dormant, its unseen events come
    /// back queued, and a note says what happened to the monitors.
    pub(crate) fn restore(&self, restored: Restored) {
        let mut state = self.lock();
        state.next_monitor = state.next_monitor.max(restored.next_monitor);
        state.next_seq = state.next_seq.max(restored.next_seq);
        let now = Instant::now();
        for monitor in restored.monitors {
            let phase = match monitor.ended {
                Some(ending) => Phase::Ended(ending),
                None => Phase::Dormant,
            };
            state.entries.push(Entry {
                id: monitor.id,
                spec: Arc::new(monitor.spec),
                run: 0,
                phase,
                started: now,
                emitted: monitor.events,
                failures: monitor.failures,
                last: None,
                task: None,
            });
        }
        for event in restored.pending {
            let ready = match event.class {
                Class::Urgent | Class::Immediate | Class::Digest => Some(now),
                Class::Boundary => None,
            };
            state.insert(Queued {
                event,
                at: now,
                ready,
            });
        }
        let dormant: Vec<(String, String, String)> = state
            .entries
            .iter()
            .filter(|entry| matches!(entry.phase, Phase::Dormant))
            // A job's wake is started again by the jobs restore, which says what became of the job.
            .filter(|entry| !matches!(entry.spec.source, Source::Job { .. }))
            .map(|entry| {
                (
                    entry.id.clone(),
                    entry.spec.label.clone(),
                    entry.spec.source.describe(),
                )
            })
            .collect();
        for (id, label, watching) in dormant {
            state.next_seq += 1;
            let seq = state.next_seq;
            state.insert(Queued {
                event: Event {
                    seq,
                    monitor: id.clone(),
                    label: label.clone(),
                    age_seconds: 0,
                    lines: vec![EventLine {
                        text: format!(
                            "`{label}` ({id}) was armed before the session was resumed and nothing is \
                             watching now (it was watching {watching}). `monitor_rearm` starts it \
                             again; `monitor_stop` forgets it."
                        ),
                        failure: false,
                    }],
                    omitted: 0,
                    failure: false,
                    ending: None,
                    class: Class::Boundary,
                },
                at: now,
                ready: None,
            });
        }
        drop(state);
        self.notify();
    }
}
