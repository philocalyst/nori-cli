//! What a monitor reports: events, and how a monitor ends.

use serde::Deserialize;
use serde::Serialize;

/// One thing a monitor saw: a burst of matching lines gathered into one, or the monitor's end.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Event {
    /// Numbers events of one actor in the order they were queued; journaled, so it never repeats.
    pub(crate) seq: i64,
    pub(crate) monitor: String,
    pub(crate) label: String,
    /// Seconds since the monitor started.
    pub(crate) age_seconds: i64,
    pub(crate) lines: Vec<EventLine>,
    /// Lines left out because the burst was longer than an event may be.
    #[serde(default)]
    pub(crate) omitted: i64,
    /// Some line is a failure, or the monitor ended in one.
    pub(crate) failure: bool,
    /// Set on the last event of a monitor.
    #[serde(default)]
    pub(crate) ending: Option<Ending>,
    pub(crate) class: Class,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EventLine {
    pub(crate) text: String,
    pub(crate) failure: bool,
}

/// How hard an event may knock on an idle agent's door.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Class {
    /// A failure, or a monitor's failed end: wakes the agent at once whatever the monitor's mode.
    Urgent,
    Immediate,
    Digest,
    Boundary,
}

/// How a monitor ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "how", rename_all = "snake_case")]
pub(crate) enum Ending {
    /// The command exited.
    Exited { code: i64 },
    /// The source finished by itself: CI decided, the actor ended.
    Finished { ok: bool, note: String },
    /// A line matched `until`.
    Until,
    /// It ran for its whole timeout without ending, after `events` events.
    Expired { events: i64 },
    /// It emitted `limit` events and was stopped.
    Flooded { limit: i64 },
    /// The source could not run, or stopped working.
    Failed { reason: String },
    /// The agent stopped it.
    Stopped,
    /// The conversation was rewound: the agent no longer knows it armed this.
    Rewound,
    /// A newer watch with the same label took its place.
    Superseded,
    /// The source finished and has nothing to tell.
    Quiet,
}

impl Ending {
    /// Whether it is bad news: the agent is woken for it.
    pub(crate) fn is_failure(&self) -> bool {
        match self {
            Self::Exited { code } => *code != 0,
            Self::Finished { ok, .. } => !ok,
            Self::Expired { .. } | Self::Flooded { .. } | Self::Failed { .. } => true,
            Self::Until | Self::Stopped | Self::Rewound | Self::Superseded | Self::Quiet => false,
        }
    }

    /// Whether the agent is told: not when it asked for the end itself, or there is nothing to say.
    pub(crate) fn is_reported(&self) -> bool {
        match self {
            Self::Exited { .. }
            | Self::Finished { .. }
            | Self::Until
            | Self::Expired { .. }
            | Self::Flooded { .. }
            | Self::Failed { .. } => true,
            Self::Stopped | Self::Rewound | Self::Superseded | Self::Quiet => false,
        }
    }

    /// The end as a status: `ended: exit 1`.
    pub(crate) fn status(&self) -> String {
        match self {
            Self::Exited { code } => format!("ended: exit {code}"),
            Self::Finished { ok: true, .. } => "ended: finished".to_owned(),
            Self::Finished { ok: false, .. } => "ended: finished, failed".to_owned(),
            Self::Until => "ended: `until` matched".to_owned(),
            Self::Expired { .. } => "ended: expired".to_owned(),
            Self::Flooded { .. } => "ended: stopped, too many events".to_owned(),
            Self::Failed { .. } => "ended: source failed".to_owned(),
            Self::Stopped => "ended: stopped".to_owned(),
            Self::Rewound => "ended: conversation rewound".to_owned(),
            Self::Superseded => "ended: replaced".to_owned(),
            Self::Quiet => "ended".to_owned(),
        }
    }

    /// What the last event says, in full. Never says more than was observed.
    pub(crate) fn sentence(&self, events: i64, failures: i64) -> String {
        let earlier = if failures > 0 && !self.is_failure() {
            format!(" {failures} earlier event(s) were failures.")
        } else {
            String::new()
        };
        match self {
            Self::Exited { code: 0 } => {
                format!("The command exited 0 after {events} event(s).{earlier}")
            }
            Self::Exited { code } => {
                format!("The command exited {code}: that is a failure. {events} event(s) in all.")
            }
            Self::Finished { ok: true, note } => format!("{note}{earlier}"),
            Self::Finished { ok: false, note } => format!("{note} That is a failure."),
            Self::Until => format!(
                "A line matched `until`; the monitor ended there after {events} event(s).{earlier}"
            ),
            Self::Expired { events } => format!(
                "The monitor expired after {events} event(s) without ending or matching `until`. \
                 That is not success: nothing was observed that says the wait is over. \
                 `monitor_rearm` watches again, or check the state directly."
            ),
            Self::Flooded { limit } => format!(
                "The monitor stopped after {limit} events (`max_events`); more may have followed. \
                 Narrow `filter`, raise `batch_ms`, or `monitor_rearm` with a higher `max_events`."
            ),
            Self::Failed { reason } => format!("The source stopped working: {reason}"),
            Self::Stopped => "Stopped.".to_owned(),
            Self::Rewound => {
                "Stopped: the conversation was rewound past where it was armed.".to_owned()
            }
            Self::Superseded => "Replaced by a newer monitor with the same label.".to_owned(),
            Self::Quiet => "Finished with nothing to report.".to_owned(),
        }
    }
}
