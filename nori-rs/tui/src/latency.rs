//! How long interactions take to show their response: the 100 ms rule, measured.
//!
//! Two things are timed, always, at the cost of an `Instant` and an uncontended lock:
//!
//! - **Stalls.** Every event the app's loop handles, by kind (`key`, `draw`, each [`AppEvent`]
//!   variant). The loop does one thing at a time, so a handler that takes 120 ms is 120 ms in
//!   which no key is read and no frame is drawn. This is what finds a blocking call on the UI
//!   path: it shows up as the event whose handler it is in.
//! - **Interactions.** From the input that asks for something to the first frame drawn after the
//!   response is on screen ([`Interaction`]). Most responses are on screen once the input is
//!   handled; one that arrives later (a picker whose rows load, a session that starts) is
//!   [`settle`]d when it arrives, and its time runs until the frame after that.
//!
//! Anything over 100 ms is logged at once (`warn`, target `nori_latency`). With
//! `NORI_LATENCY_LOG=<path>` set, the table of every path (count, median, 95th percentile,
//! worst, and how many exceeded 100 ms) is appended to that file when Nori exits; every sample is
//! also at `debug` level under the same target.
//!
//! [`AppEvent`]: crate::app_event::AppEvent

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use std::time::Instant;

/// The budget every interaction is held to.
pub(crate) const BUDGET: Duration = Duration::from_millis(100);
/// Samples kept per path for its percentiles; the counts cover every sample.
const SAMPLES_KEPT: usize = 2048;
/// An interaction still waiting for its response after this long got none (the session never
/// started, the picker was never shown): it is dropped, not timed.
const GIVE_UP: Duration = Duration::from_secs(120);

/// An interaction whose response is timed from input to frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Interaction {
    /// A key press, to the frame that shows what it did.
    Key,
    /// A slash command, to the frame that shows its response.
    Command,
    /// Opening a picker, to the frame that shows it with its rows.
    PickerOpen,
    /// Choosing a model, effort or mode, to the frame that shows it chosen (the footer). The
    /// agent's own answer comes later and is not waited for.
    ConfigSwitch,
    /// Switching to another agent, to the frame that shows the switch under way.
    AgentSwitch,
    /// A new session, to the frame that shows it ready for a prompt.
    SessionStart,
    /// Choosing a session to resume, to the frame that shows its conversation.
    Resume,
}

impl Interaction {
    fn label(self) -> &'static str {
        match self {
            Self::Key => "interaction: key → frame",
            Self::Command => "interaction: /command → frame",
            Self::PickerOpen => "interaction: picker open",
            Self::ConfigSwitch => "interaction: model/effort/mode switch",
            Self::AgentSwitch => "interaction: agent switch",
            Self::SessionStart => "interaction: session start",
            Self::Resume => "interaction: resume",
        }
    }
}

struct Open {
    interaction: Interaction,
    started: Instant,
    /// The response is on screen; the next frame ends it.
    settled: bool,
}

#[derive(Default)]
struct Samples {
    count: i64,
    over_budget: i64,
    worst: Duration,
    recent: VecDeque<Duration>,
}

impl Samples {
    fn add(&mut self, elapsed: Duration) {
        self.count += 1;
        if elapsed > BUDGET {
            self.over_budget += 1;
        }
        self.worst = self.worst.max(elapsed);
        if self.recent.len() == SAMPLES_KEPT {
            self.recent.pop_front();
        }
        self.recent.push_back(elapsed);
    }

    fn percentile(&self, percent: i64) -> Duration {
        let mut sorted: Vec<Duration> = self.recent.iter().copied().collect();
        sorted.sort_unstable();
        let last = i64::try_from(sorted.len()).unwrap_or(i64::MAX) - 1;
        usize::try_from(last * percent / 100)
            .ok()
            .and_then(|index| sorted.get(index).copied())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct Table {
    open: Vec<Open>,
    paths: BTreeMap<&'static str, Samples>,
}

fn table() -> std::sync::MutexGuard<'static, Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn record(label: &'static str, elapsed: Duration) {
    if elapsed > BUDGET {
        tracing::warn!(
            target: "nori_latency",
            path = label,
            elapsed_ms = elapsed.as_millis(),
            "over the 100 ms budget"
        );
    } else {
        tracing::debug!(target: "nori_latency", path = label, elapsed_us = elapsed.as_micros());
    }
    table().paths.entry(label).or_default().add(elapsed);
}

/// The app's loop handled an event of kind `kind` in `elapsed`.
pub(crate) fn handled(kind: &'static str, elapsed: Duration) {
    record(kind, elapsed);
}

/// `interaction` was asked for now. It ends at the first frame drawn after it is [`settle`]d, or,
/// for one that `settles_at_once`, at the next frame.
pub(crate) fn begin(interaction: Interaction, settles_at_once: bool) {
    table().open.push(Open {
        interaction,
        started: Instant::now(),
        settled: settles_at_once,
    });
}

/// The response to the oldest open `interaction` is on screen: the next frame ends it.
pub(crate) fn settle(interaction: Interaction) {
    if let Some(open) = table()
        .open
        .iter_mut()
        .find(|open| open.interaction == interaction && !open.settled)
    {
        open.settled = true;
    }
}

/// An interaction that was asked for and will not get a response (the picker was dismissed, the
/// switch was refused): it is not timed.
pub(crate) fn abandon(interaction: Interaction) {
    let mut table = table();
    if let Some(position) = table
        .open
        .iter()
        .position(|open| open.interaction == interaction)
    {
        table.open.remove(position);
    }
}

/// A frame was drawn: every settled interaction ends now.
pub(crate) fn frame_drawn() {
    let now = Instant::now();
    let ended: Vec<(Interaction, Duration)> = {
        let mut table = table();
        let mut ended = Vec::new();
        table.open.retain(|open| {
            if open.settled {
                ended.push((open.interaction, now.duration_since(open.started)));
            }
            !open.settled && now.duration_since(open.started) < GIVE_UP
        });
        ended
    };
    for (interaction, elapsed) in ended {
        record(interaction.label(), elapsed);
    }
}

/// The table of every path measured so far, slowest worst case first.
pub(crate) fn report() -> String {
    let table = table();
    let mut rows: Vec<(&&'static str, &Samples)> = table.paths.iter().collect();
    rows.sort_by_key(|row| std::cmp::Reverse(row.1.worst));
    let mut out = format!(
        "{:<52} {:>7} {:>9} {:>9} {:>9} {:>7}\n",
        "path", "count", "p50 ms", "p95 ms", "max ms", ">100ms"
    );
    let ms = |duration: Duration| duration.as_secs_f64() * 1000.0;
    for (label, samples) in rows {
        let _ = writeln!(
            out,
            "{label:<52} {:>7} {:>9.2} {:>9.2} {:>9.2} {:>7}",
            samples.count,
            ms(samples.percentile(50)),
            ms(samples.percentile(95)),
            ms(samples.worst),
            samples.over_budget,
        );
    }
    out
}

/// Append the table to `NORI_LATENCY_LOG`, when it is set. Called once, as Nori exits.
pub(crate) fn write_report() {
    let Some(path) = std::env::var_os("NORI_LATENCY_LOG") else {
        return;
    };
    let report = report();
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, report.as_bytes()));
    if let Err(error) = written {
        tracing::warn!(target: "nori_latency", %error, "could not write the latency table");
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn an_interaction_ends_at_the_frame_after_it_settled() {
        begin(Interaction::Resume, false);
        frame_drawn();
        // Not settled: the frame did not show its response.
        assert!(!report().contains(Interaction::Resume.label()));
        settle(Interaction::Resume);
        frame_drawn();
        let row = report()
            .lines()
            .find(|line| line.starts_with(Interaction::Resume.label()))
            .map(|line| line.split_whitespace().count());
        // label words + count, p50, p95, max, >100ms
        assert_eq!(
            row,
            Some(Interaction::Resume.label().split_whitespace().count() + 5)
        );
    }
}
