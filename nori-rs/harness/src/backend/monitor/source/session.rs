//! The `session` source: what happens in the session itself, as events.
//!
//! It reads the session's public event stream and reports the moments a monitor can act on: a turn
//! ended or failed, the session moved to another route, a subagent ended, the agent asked for
//! permission, and the session stayed idle for a while. Turns that a monitor's own wake started are
//! not "turns that ended": a monitor that wakes on `turn_ended` must not wake itself forever.

use std::collections::HashMap;
use std::time::Duration;

use nori_protocol::AcpEvent;
use nori_protocol::MonitorWire;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SessionPhase;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::UsageReport;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::monitor::event::Ending;
use crate::backend::monitor::spec::SessionTrigger;

/// Where the session's turns stand, as the event stream shows them.
#[derive(Default)]
struct Turns {
    working: bool,
    /// The running turn was started by a monitor's wake.
    woken: bool,
    idle_since: Option<tokio::time::Instant>,
    idle_reported: bool,
    /// The highest level of [`QUOTA_LEVELS`] reported per usage window, by window id; a window
    /// whose use falls back below every level (it reset) can be reported again.
    quota: HashMap<String, i64>,
}

/// How full a usage window is when `quota_low` speaks: percent used.
const QUOTA_LEVELS: [i64; 3] = [80, 95, 100];

pub(super) async fn run(
    mut events: Receiver<SessionEvent>,
    triggers: &[SessionTrigger],
    idle_after: Option<Duration>,
    tx: &Sender<Item>,
) -> Option<Ending> {
    let wants = |trigger: SessionTrigger| triggers.contains(&trigger);
    let mut turns = Turns {
        idle_since: Some(tokio::time::Instant::now()),
        ..Turns::default()
    };
    loop {
        let idle_due = async {
            match (idle_after, turns.idle_since) {
                (Some(after), Some(since)) if !turns.idle_reported => {
                    tokio::time::sleep_until(since + after).await;
                }
                _ => std::future::pending().await,
            }
        };
        let line = tokio::select! {
            event = events.recv() => match event {
                Some(event) => observe(&event, &mut turns, &wants),
                None => {
                    return Some(Ending::Finished {
                        ok: true,
                        note: "The session ended.".to_owned(),
                    });
                }
            },
            () = idle_due => {
                turns.idle_reported = true;
                idle_after.map(|after| format!("the session has been idle for {}s", after.as_secs()))
            }
        };
        if let Some(text) = line
            && tx
                .send(Item::Line {
                    text,
                    failure: false,
                })
                .await
                .is_err()
        {
            return None;
        }
    }
}

/// The line `event` is worth to the monitor, if any, keeping track of the turns.
fn observe(
    event: &SessionEvent,
    turns: &mut Turns,
    wants: &impl Fn(SessionTrigger) -> bool,
) -> Option<String> {
    match event {
        SessionEvent::Nori(NoriEvent::SessionPhaseChanged(phase)) => {
            let idle = matches!(phase, SessionPhase::Idle);
            if !idle {
                turns.working = true;
                turns.idle_since = None;
                return None;
            }
            if !std::mem::take(&mut turns.working) {
                return None;
            }
            turns.idle_since = Some(tokio::time::Instant::now());
            turns.idle_reported = false;
            let woken = std::mem::take(&mut turns.woken);
            (wants(SessionTrigger::TurnEnded) && !woken).then(|| "a turn ended".to_owned())
        }
        SessionEvent::Nori(NoriEvent::Monitor(MonitorWire::Delivered(_))) => {
            turns.woken = true;
            None
        }
        SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => wants(SessionTrigger::TurnFailed)
            .then(|| format!("a request failed: {}", failure.message)),
        SessionEvent::Nori(NoriEvent::RouteChanged(change)) => wants(SessionTrigger::RouteChanged)
            .then(|| format!("the session moved from {} to {}", change.from, change.to)),
        SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) => {
            let Patch::Set(SubagentState::Ended(outcome)) = &update.state else {
                return None;
            };
            let who = match &update.name {
                Patch::Set(name) => name.clone(),
                Patch::Keep | Patch::Clear => update.session_id.to_string(),
            };
            let how = match outcome {
                SubagentOutcome::Completed { .. } => "completed".to_owned(),
                SubagentOutcome::Failed { reason } => match reason {
                    Some(reason) => format!("failed: {reason}"),
                    None => "failed".to_owned(),
                },
                SubagentOutcome::Cancelled => "was cancelled".to_owned(),
                SubagentOutcome::Closed => "was closed".to_owned(),
            };
            wants(SessionTrigger::SubagentEnded).then(|| format!("subagent {who} {how}"))
        }
        SessionEvent::Acp(AcpEvent::Notification(acp::AgentNotification::SessionNotification(
            notification,
        ))) if wants(SessionTrigger::QuotaLow) => {
            let acp::SessionUpdate::UsageUpdate(usage) = &notification.update else {
                return None;
            };
            let report = usage.meta.as_ref().and_then(UsageReport::from_meta)?;
            let lines: Vec<String> = report
                .rate_limits
                .iter()
                .filter_map(|window| quota_line(window, &mut turns.quota))
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        SessionEvent::Acp(AcpEvent::Request {
            request: acp::AgentRequest::RequestPermissionRequest(request),
            ..
        }) => wants(SessionTrigger::PermissionRequested).then(|| {
            format!(
                "the agent asked for permission: {}",
                request
                    .tool_call
                    .fields
                    .title
                    .as_deref()
                    .unwrap_or("a tool")
            )
        }),
        _ => None,
    }
}

/// What `window` is worth saying, once per level it reaches; `reported` remembers the levels said.
fn quota_line(
    window: &nori_protocol::RateLimitWindow,
    reported: &mut HashMap<String, i64>,
) -> Option<String> {
    // Whole percent, rounded down: 99.6% used is not yet spent.
    let used = window.used_percent.floor() as i64;
    let Some(level) = QUOTA_LEVELS
        .iter()
        .copied()
        .filter(|level| used >= *level)
        .max()
    else {
        // Below every level: the window reset, so it may speak again.
        reported.remove(&window.id);
        return None;
    };
    if reported
        .get(&window.id)
        .is_some_and(|before| *before >= level)
    {
        return None;
    }
    reported.insert(window.id.clone(), level);
    let resets = window
        .resets_at
        .as_deref()
        .map(|at| format!(", resets at {at}"))
        .unwrap_or_default();
    Some(if level >= 100 {
        format!(
            "quota spent: the {} window is used up{resets}",
            window.label
        )
    } else {
        format!(
            "quota low: the {} window is {used}% used{resets}",
            window.label
        )
    })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn window(used_percent: f64) -> nori_protocol::RateLimitWindow {
        nori_protocol::RateLimitWindow {
            id: "five_hour".to_owned(),
            label: "5h".to_owned(),
            kind: nori_protocol::RateLimitKind::Session,
            used_percent,
            resets_at: Some("2026-11-20T03:00:00Z".to_owned()),
            window_minutes: Some(300),
            model_ids: Vec::new(),
        }
    }

    #[test]
    fn each_level_is_said_once_until_the_window_resets() {
        let mut reported = HashMap::new();
        let said: Vec<Option<String>> = [50.0, 81.2, 84.0, 96.0, 99.9, 100.0, 100.0, 10.0, 82.0]
            .into_iter()
            .map(|used| quota_line(&window(used), &mut reported))
            .collect();
        let resets = ", resets at 2026-11-20T03:00:00Z";
        assert_eq!(
            said,
            vec![
                None,
                Some(format!("quota low: the 5h window is 81% used{resets}")),
                None,
                Some(format!("quota low: the 5h window is 96% used{resets}")),
                None,
                Some(format!("quota spent: the 5h window is used up{resets}")),
                None,
                None,
                Some(format!("quota low: the 5h window is 82% used{resets}")),
            ]
        );
    }
}
