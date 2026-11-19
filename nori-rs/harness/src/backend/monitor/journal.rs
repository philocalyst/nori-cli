//! A session's monitors in its conversation journal, so a resume restores them.
//!
//! Everything is one small `monitor` command: a monitor armed, an event queued, a batch of events
//! offered to the agent, the batch delivered, events consumed by a tool result, a monitor ended.
//! Folding them in order says which monitors were armed and which events nobody has seen.
//!
//! An event queued is journaled before the agent can see it. It counts as seen when its batch was
//! delivered, a tool result carried it, or the conversation holds the batch's message: a crash
//! between the agent receiving a batch and the journal saying so neither loses nor repeats it.
//! Rewound records are not visible to the fold, so a rewind gives back what it unsaid.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;

use serde::Deserialize;
use serde::Serialize;

use super::event::Ending;
use super::event::Event;
use super::spec::Spec;

/// The command name of every record.
pub(crate) const COMMAND: &str = "monitor";
/// Ended monitors kept in a restored list.
const KEEP_ENDED: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum Record {
    /// A monitor armed, or armed again under the same id.
    Armed {
        id: String,
        spec: Spec,
    },
    Event {
        event: Event,
    },
    /// These events were offered to the agent in the message named `batch`.
    Batch {
        batch: String,
        seqs: Vec<i64>,
    },
    Delivered {
        batch: String,
    },
    /// These events were read out of a tool result, or dropped; nobody is owed them.
    Taken {
        seqs: Vec<i64>,
    },
    Ended {
        id: String,
        ending: Ending,
    },
}

/// One monitor as the journal left it.
#[derive(Debug, Clone)]
pub(crate) struct RestoredMonitor {
    pub(crate) id: String,
    pub(crate) spec: Spec,
    /// How it ended; `None` means it was still armed when the journal ended.
    pub(crate) ended: Option<Ending>,
    pub(crate) events: i64,
    pub(crate) failures: i64,
}

#[derive(Debug, Default)]
pub(crate) struct Restored {
    pub(crate) monitors: Vec<RestoredMonitor>,
    /// Events the agent has not seen, oldest first.
    pub(crate) pending: Vec<Event>,
    pub(crate) next_monitor: i64,
    pub(crate) next_seq: i64,
}

/// What `records` leave armed and unseen. `in_conversation` holds the ids of the batches the
/// conversation itself contains.
pub(crate) fn fold(
    records: impl IntoIterator<Item = Record>,
    in_conversation: &HashSet<String>,
) -> Restored {
    let mut monitors: Vec<RestoredMonitor> = Vec::new();
    let mut events: BTreeMap<i64, Event> = BTreeMap::new();
    let mut batches: HashMap<String, Vec<i64>> = HashMap::new();
    let mut seen: HashSet<i64> = HashSet::new();
    let mut next_seq = 0;
    for record in records {
        match record {
            Record::Armed { id, spec } => {
                match monitors.iter_mut().find(|monitor| monitor.id == id) {
                    Some(monitor) => {
                        monitor.spec = spec;
                        monitor.ended = None;
                    }
                    None => monitors.push(RestoredMonitor {
                        id,
                        spec,
                        ended: None,
                        events: 0,
                        failures: 0,
                    }),
                }
            }
            Record::Event { event } => {
                next_seq = next_seq.max(event.seq);
                if let Some(monitor) = monitors.iter_mut().find(|m| m.id == event.monitor) {
                    monitor.events += 1;
                    monitor.failures += i64::from(event.failure);
                }
                events.insert(event.seq, event);
            }
            Record::Batch { batch, seqs } => {
                next_seq = next_seq.max(seqs.iter().copied().max().unwrap_or(0));
                batches.insert(batch, seqs);
            }
            Record::Delivered { batch } => {
                seen.extend(batches.get(&batch).into_iter().flatten().copied());
            }
            Record::Taken { seqs } => seen.extend(seqs),
            Record::Ended { id, ending } => {
                if let Some(monitor) = monitors.iter_mut().find(|monitor| monitor.id == id) {
                    monitor.ended = Some(ending);
                }
            }
        }
    }
    for batch in in_conversation {
        seen.extend(batches.get(batch).into_iter().flatten().copied());
    }
    let next_monitor = monitors
        .iter()
        .filter_map(|monitor| monitor.id.strip_prefix("mon-")?.parse::<i64>().ok())
        .max()
        .unwrap_or(0);
    let ended = monitors
        .iter()
        .filter(|monitor| monitor.ended.is_some())
        .count();
    let mut surplus = ended.saturating_sub(KEEP_ENDED);
    monitors.retain(|monitor| {
        if monitor.ended.is_some() && surplus > 0 {
            surplus -= 1;
            false
        } else {
            true
        }
    });
    Restored {
        monitors,
        pending: events
            .into_values()
            .filter(|event| !seen.contains(&event.seq))
            .collect(),
        next_monitor,
        next_seq,
    }
}

/// The ids of the batches a conversation's text contains: each delivered batch names itself in
/// its message (`<monitor_events batch="...">`).
pub(crate) fn batches_in(texts: impl IntoIterator<Item = String>) -> HashSet<String> {
    let mut found = HashSet::new();
    for text in texts {
        let mut rest = text.as_str();
        while let Some(start) = rest.find("<monitor_events batch=\"") {
            rest = &rest[start + "<monitor_events batch=\"".len()..];
            if let Some(end) = rest.find('"') {
                found.insert(rest[..end].to_owned());
            }
        }
    }
    found
}
