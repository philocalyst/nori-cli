//! From a source's raw lines to a monitor's events.
//!
//! One task per running monitor. It runs the source in a task of its own (killed when this one
//! ends), reads its items, and applies the rules: `fail_on` lines always count and are marked
//! failures, `filter` drops the rest that do not match, `until` ends the monitor after its line.
//! Lines that arrive close together are one event. A monitor that keeps emitting grows its window
//! so a flood becomes fewer, larger events, and past `max_events` it is stopped and says so.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;

use super::event::Ending;
use super::event::EventLine;
use super::registry::Burst;
use super::registry::Monitors;
use super::source;
use super::source::Item;
use super::source::SourceEnv;
use super::spec::CompiledRules;
use super::spec::Spec;

/// The most lines of each kind one event carries; more are counted, not kept.
const MAX_LINES: usize = 40;
/// The longest a line is kept.
const MAX_LINE_CHARS: usize = 2000;
/// The longest the window of a flooding monitor grows to.
const MAX_WINDOW: Duration = Duration::from_secs(5);
/// Events this close together, five in a row, mean a flood.
const FLOOD_SPAN: Duration = Duration::from_secs(10);

/// Aborts its task when dropped, so a source never outlives the monitor that runs it.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Default)]
struct Batch {
    info: Vec<EventLine>,
    failures: Vec<EventLine>,
    omitted: i64,
}

impl Batch {
    fn is_empty(&self) -> bool {
        self.info.is_empty() && self.failures.is_empty() && self.omitted == 0
    }

    fn push(&mut self, line: EventLine) {
        let kept = if line.failure {
            &mut self.failures
        } else {
            &mut self.info
        };
        if kept.len() < MAX_LINES {
            kept.push(line);
        } else {
            self.omitted += 1;
        }
    }

    /// The batch as one burst, failures first: what matters is never cut for what does not.
    fn take(&mut self) -> Burst {
        let batch = std::mem::take(self);
        let mut lines = batch.failures;
        lines.extend(batch.info);
        Burst {
            lines,
            omitted: batch.omitted,
        }
    }
}

pub(super) async fn run(
    monitors: Monitors,
    id: String,
    run: i64,
    spec: Arc<Spec>,
    rules: CompiledRules,
    env: SourceEnv,
) {
    let (tx, mut items) = mpsc::channel::<Item>(256);
    let _source = AbortOnDrop(tokio::spawn(source::run(spec.source.clone(), env, tx)));
    let base_window = Duration::from_millis(spec.limits.batch_ms.max(0).unsigned_abs());
    let mut window = base_window;
    let mut flushed: VecDeque<Instant> = VecDeque::new();
    let mut batch = Batch::default();
    let mut flush_at: Option<Instant> = None;
    let mut emitted = 0;
    let Some(mut deadline) = monitors.deadline(&id, run) else {
        return;
    };

    let ending = loop {
        let flush_due = async {
            match flush_at {
                Some(at) => tokio::time::sleep_until(at.into()).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            item = items.recv() => match item {
                Some(item @ (Item::Line { .. } | Item::Baseline { .. })) => {
                    let (text, failure, baseline) = match item {
                        Item::Line { text, failure } => (text, failure, false),
                        Item::Baseline { text } => (text, false, true),
                        Item::Done(_) => continue,
                    };
                    let failed = failure || rules.fail_on.iter().any(|re| re.is_match(&text));
                    let last = rules.until.as_ref().is_some_and(|re| re.is_match(&text));
                    let wanted = failed
                        || last
                        || (!baseline
                            && rules.filter.as_ref().is_none_or(|re| re.is_match(&text)));
                    if !wanted {
                        continue;
                    }
                    batch.push(EventLine { text: clip(text), failure: failed });
                    if last {
                        break Ending::Until;
                    }
                    flush_at.get_or_insert_with(|| Instant::now() + window);
                }
                Some(Item::Done(ending)) => break ending,
                None => {
                    break Ending::Failed {
                        reason: "the source stopped without saying why".to_owned(),
                    };
                }
            },
            () = flush_due => {
                flush_at = None;
                if !emit(&monitors, &id, run, &mut batch).await {
                    return;
                }
                emitted += 1;
                let now = Instant::now();
                flushed.push_back(now);
                if flushed.len() > 5 {
                    flushed.pop_front();
                }
                // Five events inside ten seconds: gather longer, so a flood is a few big events.
                window = if flushed.len() == 5
                    && flushed.front().is_some_and(|first| now.duration_since(*first) < FLOOD_SPAN)
                {
                    (window * 2).min(MAX_WINDOW)
                } else {
                    base_window
                };
                if emitted >= spec.limits.max_events {
                    break Ending::Flooded { limit: spec.limits.max_events };
                }
            }
            () = tokio::time::sleep_until(deadline.into()) => {
                // A re-arm may have moved the deadline: only the monitor's own clock decides.
                match monitors.deadline(&id, run) {
                    Some(later) if later > Instant::now() => deadline = later,
                    Some(_) => break Ending::Expired { events: 0 },
                    None => return,
                }
            }
        }
    };
    // What was gathered when the source ended goes out with its end: one moment, one event.
    let burst = (!batch.is_empty()).then(|| batch.take());
    if burst.is_some() {
        emitted += 1;
    }
    let ending = match ending {
        Ending::Expired { .. } if spec.source.expires_quietly() => Ending::Finished {
            ok: true,
            note: format!("The watch ran its course after {emitted} event(s)."),
        },
        Ending::Expired { .. } => Ending::Expired { events: emitted },
        other => other,
    };
    monitors.finish(&id, run, burst, ending).await;
}

async fn emit(monitors: &Monitors, id: &str, run: i64, batch: &mut Batch) -> bool {
    if batch.is_empty() {
        return true;
    }
    monitors.emit(id, run, batch.take()).await
}

fn clip(text: String) -> String {
    if text.chars().count() <= MAX_LINE_CHARS {
        return text;
    }
    let mut clipped: String = text.chars().take(MAX_LINE_CHARS).collect();
    clipped.push('…');
    clipped
}
