//! What the agent is told about monitors: the wake message, the events a tool result carries, and
//! the digest a client shows. Every string says what was observed and what to do next.

use super::event::Event;
use super::lease::Lease;
use nori_protocol::MonitorDigest;

/// A wake message holds at most this much event text; failures are kept first.
const WAKE_BYTES: usize = 24 * 1024;

/// The hidden prompt that hands an agent the events of its monitors.
pub(super) fn wake_text(lease: &Lease) -> String {
    let mut text = format!(
        "<monitor_events batch=\"{}\">\nMonitors you armed reported while you were not waiting. \
         Lines marked FAILED are failures: deal with them first. A monitor keeps running unless it \
         says it ended; do not poll it, and end your turn when you have nothing else to do.\n",
        lease.batch
    );
    let mut budget = WAKE_BYTES;
    let mut omitted_events = 0;
    let mut shown: Vec<&Event> = Vec::new();
    // Failures first, then the newest: what matters survives a long backlog.
    let mut order: Vec<&Event> = lease.events.iter().collect();
    order.sort_by_key(|event| (!event.failure, std::cmp::Reverse(event.seq)));
    for event in order {
        let size = event
            .lines
            .iter()
            .map(|line| line.text.len() + 16)
            .sum::<usize>();
        if size <= budget {
            budget -= size;
            shown.push(event);
        } else {
            omitted_events += 1;
        }
    }
    shown.sort_by_key(|event| event.seq);
    for monitor in &lease.monitors {
        text.push_str(&format!(
            "\n<monitor id=\"{}\" label=\"{}\" source=\"{}\" status=\"{}\">\n",
            monitor.id,
            escape(&monitor.label),
            monitor.source,
            escape(&monitor.status)
        ));
        for event in shown.iter().filter(|event| event.monitor == monitor.id) {
            text.push_str(&render(event));
        }
        if monitor.rearmable {
            text.push_str("It is not running. `monitor_rearm` starts it again.\n");
        }
        text.push_str("</monitor>\n");
    }
    if omitted_events > 0 {
        text.push_str(&format!(
            "\n{omitted_events} older event(s) did not fit and are not shown.\n"
        ));
    }
    if lease.dropped > 0 {
        text.push_str(&format!(
            "\n{} event(s) were dropped because too many were waiting; the monitors saw more than \
             this shows. Narrow `filter` or use `wake: digest`.\n",
            lease.dropped
        ));
    }
    text.push_str("</monitor_events>");
    text
}

/// One event as rows: the first carries its number and age, failures are marked.
fn render(event: &Event) -> String {
    let mut out = String::new();
    for (index, line) in event.lines.iter().enumerate() {
        let head = if index == 0 {
            format!("#{} +{}s", event.seq, event.age_seconds)
        } else {
            format!("#{}", event.seq)
        };
        let mark = if line.failure { "FAILED " } else { "" };
        out.push_str(&format!("{head} {mark}{}\n", line.text));
    }
    if event.omitted > 0 {
        out.push_str(&format!(
            "#{} ({} more line(s) in this burst were not kept)\n",
            event.seq, event.omitted
        ));
    }
    out
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

/// Events as a tool result carries them.
pub(crate) fn events_json(events: &[Event]) -> serde_json::Value {
    events
        .iter()
        .map(|event| {
            serde_json::json!({
                "event": event.seq,
                "monitor": event.monitor,
                "label": event.label,
                "after_seconds": event.age_seconds,
                "failure": event.failure,
                "lines": event.lines.iter().map(|line| if line.failure {
                    format!("FAILED {}", line.text)
                } else {
                    line.text.clone()
                }).collect::<Vec<_>>(),
                "omitted_lines": event.omitted,
                "ended": event.ending.as_ref().map(super::event::Ending::status),
            })
        })
        .collect::<Vec<_>>()
        .into()
}

/// What each monitor in a lease contributed, for the client's one-line cards.
pub(super) fn digests(lease: &Lease) -> Vec<MonitorDigest> {
    lease
        .monitors
        .iter()
        .map(|monitor| {
            let mine: Vec<&Event> = lease
                .events
                .iter()
                .filter(|event| event.monitor == monitor.id)
                .collect();
            let headline = mine
                .iter()
                .flat_map(|event| event.lines.iter())
                .find(|line| line.failure)
                .or_else(|| mine.last().and_then(|event| event.lines.last()))
                .map(|line| line.text.clone())
                .unwrap_or_default();
            MonitorDigest {
                label: monitor.label.clone(),
                events: i64::try_from(mine.len()).unwrap_or(i64::MAX),
                failed: mine.iter().any(|event| event.failure),
                headline,
                ended: mine
                    .iter()
                    .find_map(|event| event.ending.as_ref())
                    .map(super::event::Ending::status),
            }
        })
        .collect()
}
