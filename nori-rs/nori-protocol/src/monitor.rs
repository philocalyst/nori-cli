//! Monitors: background watchers an agent armed, as a client shows them.
//!
//! An agent arms a monitor (a command, a file, a CI run, a peer) and ends its turn; what the
//! monitor sees reaches it as a new message. The client is told two things: how many monitors are
//! armed, and each batch of events handed to the agent.

use serde::Deserialize;
use serde::Serialize;

/// What the harness tells a client about the session's monitors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MonitorWire {
    /// The set of armed monitors changed.
    State(MonitorsState),
    /// A batch of events was handed to the agent in a turn of its own.
    Delivered(MonitorsDelivered),
}

/// The monitors armed for a session, published whenever that changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorsState {
    /// Monitors whose source is running.
    pub watching: i64,
    /// Monitors restored from an earlier run: armed in the journal, with nothing watching until
    /// the agent re-arms them.
    pub dormant: i64,
    /// The labels of the running monitors, oldest first.
    pub labels: Vec<String>,
    /// Background jobs the session follows that have not ended (running, or running on a machine
    /// that does not answer right now).
    #[serde(default)]
    pub jobs: i64,
}

/// One delivery of monitor events to the agent that armed them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorsDelivered {
    pub monitors: Vec<MonitorDigest>,
}

/// What one monitor contributed to a delivery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorDigest {
    pub label: String,
    /// Events of this monitor in the delivery.
    pub events: i64,
    /// Whether any of them is a failure, or the monitor ended in one.
    pub failed: bool,
    /// The most important line: the first failure, else the last event.
    pub headline: String,
    /// How the monitor ended, when this delivery carries its end (`exit 1`, `expired`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
}
