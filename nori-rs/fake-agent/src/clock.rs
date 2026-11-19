//! Time for a world: virtual and deterministic in-process, real when served to a client.
//!
//! In virtual mode no timer ever runs. Every task that scripts time (a turn, a detached tail, a
//! parallel lane, a delayed response) is a [`Participant`]. When every participant is asleep, the
//! clock jumps straight to the earliest deadline and wakes that one task alone, so thirty minutes
//! of scripted work finish in microseconds and run in the same order every time. Waiting on the
//! outside world (a permission answer, a host tool) is [`Participant::external`]: the participant
//! stops counting as running, so time may pass for the others, and starts counting again when the
//! answer arrives.
//!
//! An external wait is "fresh" for a short real-time grace: the outside world is assumed to
//! answer within it, at the same instant of the world's time, so time does not run ahead of an
//! answer that is a few milliseconds away. A wait that outlasts the grace (a question nobody
//! answers) stops holding time back, so timeouts and the rest of the script still happen.
//!
//! Ties between equal deadlines break by the order the sleeps began.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use tokio::sync::oneshot;

/// How long, in real time, the outside world is given to answer before time runs on.
const EXTERNAL_GRACE: Duration = Duration::from_millis(300);

/// How a world's time passes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClockMode {
    /// Deterministic, instant. For driving the harness in-process.
    Virtual,
    /// Real time, scaled: `Real(1.0)` is wall-clock, `Real(10.0)` runs ten times faster. For a
    /// served process that a TUI or a PTY test watches.
    Real(f64),
}

impl ClockMode {
    /// `virtual`, `real`, `x10` (ten times faster than real), `x0.5`.
    pub fn parse(text: &str) -> Result<Self, String> {
        match text {
            "virtual" => Ok(Self::Virtual),
            "real" => Ok(Self::Real(1.0)),
            other => other
                .strip_prefix('x')
                .and_then(|speed| speed.parse::<f64>().ok())
                .filter(|speed| speed.is_finite() && *speed > 0.0)
                .map(Self::Real)
                .ok_or_else(|| {
                    format!("clock {other:?} is not `virtual`, `real`, or a speed such as `x10`")
                }),
        }
    }
}

#[derive(Default)]
struct State {
    elapsed: Duration,
    running: i64,
    /// External waits still inside their grace.
    fresh: i64,
    next: u64,
    sleepers: BTreeMap<(Duration, u64), oneshot::Sender<()>>,
}

impl State {
    /// Wake the earliest sleeper if nobody is running and no answer is about to arrive.
    fn advance(&mut self) {
        if self.running == 0
            && self.fresh == 0
            && let Some(((deadline, _), wake)) = self.sleepers.pop_first()
        {
            self.elapsed = self.elapsed.max(deadline);
            self.running += 1;
            let _ = wake.send(());
        }
    }
}

struct Inner {
    mode: ClockMode,
    start_unix: i64,
    started: Instant,
    state: Mutex<State>,
}

/// A world's clock. Cheap to clone; clones share time.
#[derive(Clone)]
pub struct Clock {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Clock {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Clock")
            .field("mode", &self.inner.mode)
            .field("elapsed", &self.elapsed())
            .finish()
    }
}

impl Clock {
    /// A clock whose time zero is `start_unix` seconds since the epoch.
    pub fn new(mode: ClockMode, start_unix: i64) -> Self {
        Self {
            inner: Arc::new(Inner {
                mode,
                start_unix,
                started: Instant::now(),
                state: Mutex::new(State::default()),
            }),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn mode(&self) -> ClockMode {
        self.inner.mode
    }

    /// Time since the world started, in the world's own time.
    pub fn elapsed(&self) -> Duration {
        match self.inner.mode {
            ClockMode::Virtual => self.state().elapsed,
            ClockMode::Real(speed) => self.inner.started.elapsed().mul_f64(speed),
        }
    }

    /// The world's now, in seconds since the Unix epoch.
    pub fn now_unix(&self) -> i64 {
        self.inner.start_unix + i64::try_from(self.elapsed().as_secs()).unwrap_or(i64::MAX / 2)
    }

    /// The world's now, RFC 3339 in UTC.
    pub fn now_rfc3339(&self) -> String {
        nori_acp_kit::rfc3339_utc(self.now_unix())
    }

    /// `after` from now, RFC 3339 in UTC.
    pub fn rfc3339_in(&self, after: Duration) -> String {
        nori_acp_kit::rfc3339_utc(
            self.now_unix() + i64::try_from(after.as_secs()).unwrap_or(i64::MAX / 2),
        )
    }

    /// Take part in the clock. Time advances only while every participant sleeps or waits
    /// outside; a task that scripts time holds a participant for as long as it runs.
    pub fn join(&self) -> Participant {
        if self.inner.mode == ClockMode::Virtual {
            self.state().running += 1;
        }
        Participant {
            clock: self.clone(),
        }
    }
}

/// A task's share in a virtual clock. Dropping it leaves the clock.
pub struct Participant {
    clock: Clock,
}

impl std::fmt::Debug for Participant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Participant")
            .finish_non_exhaustive()
    }
}

/// While alive, the participant is waiting on the outside world and does not hold time back.
struct Away {
    clock: Clock,
    /// Whether the wait is still inside its grace; whoever clears it adjusts `fresh`.
    fresh: Arc<AtomicBool>,
}

impl Drop for Away {
    fn drop(&mut self) {
        let mut state = self.clock.state();
        if self.fresh.swap(false, Ordering::SeqCst) {
            state.fresh -= 1;
        }
        state.running += 1;
    }
}

/// A sleep that is cancelled (its future dropped) takes its entry back.
struct SleepEntry {
    clock: Clock,
    key: (Duration, u64),
}

impl Drop for SleepEntry {
    fn drop(&mut self) {
        let mut state = self.clock.state();
        if state.sleepers.remove(&self.key).is_some() {
            // Never woken: this participant is running again.
            state.running += 1;
        }
        // Otherwise the clock woke it and already counted it as running.
    }
}

impl Participant {
    /// Another participant in the same clock, for a task or lane this one starts.
    pub fn join(&self) -> Self {
        self.clock.join()
    }

    pub fn clock(&self) -> &Clock {
        &self.clock
    }

    /// Sleep for `duration` of the world's time.
    pub async fn sleep(&self, duration: Duration) {
        match self.clock.inner.mode {
            ClockMode::Real(speed) => tokio::time::sleep(duration.div_f64(speed)).await,
            ClockMode::Virtual if duration.is_zero() => tokio::task::yield_now().await,
            ClockMode::Virtual => {
                let (wake, woken) = oneshot::channel();
                let key = {
                    let mut state = self.clock.state();
                    let key = (state.elapsed + duration, state.next);
                    state.next += 1;
                    state.sleepers.insert(key, wake);
                    state.running -= 1;
                    state.advance();
                    key
                };
                let _entry = SleepEntry {
                    clock: self.clock.clone(),
                    key,
                };
                let _ = woken.await;
            }
        }
    }

    /// Wait for something outside the world (a person's answer, Nori's reply to a host tool).
    /// Time may pass for the others meanwhile.
    pub async fn external<F: Future>(&self, future: F) -> F::Output {
        let _away = self.away();
        future.await
    }

    /// Wait for tasks of this same world to finish (parallel lanes), or for something that never
    /// ends: this task stops holding time back at once, with no grace.
    pub async fn park<F: Future>(&self, future: F) -> F::Output {
        let _parked = if self.clock.inner.mode == ClockMode::Virtual {
            let mut state = self.clock.state();
            state.running -= 1;
            state.advance();
            Some(Away {
                clock: self.clock.clone(),
                fresh: Arc::new(AtomicBool::new(false)),
            })
        } else {
            None
        };
        future.await
    }

    fn away(&self) -> Option<Away> {
        if self.clock.inner.mode != ClockMode::Virtual {
            return None;
        }
        let fresh = Arc::new(AtomicBool::new(true));
        {
            let mut state = self.clock.state();
            state.running -= 1;
            state.fresh += 1;
        }
        let clock = self.clock.clone();
        let flag = Arc::clone(&fresh);
        tokio::spawn(async move {
            tokio::time::sleep(EXTERNAL_GRACE).await;
            if flag.swap(false, Ordering::SeqCst) {
                let mut state = clock.state();
                state.fresh -= 1;
                state.advance();
            }
        });
        Some(Away {
            clock: self.clock.clone(),
            fresh,
        })
    }
}

impl Drop for Participant {
    fn drop(&mut self) {
        if self.clock.inner.mode == ClockMode::Virtual {
            let mut state = self.clock.state();
            state.running -= 1;
            state.advance();
        }
    }
}

/// `2026-11-18T12:00:00Z` (or `.fff` seconds, or `+00:00`) as seconds since the Unix epoch.
pub fn parse_rfc3339_utc(text: &str) -> Option<i64> {
    let text = text.trim();
    let text = text
        .strip_suffix('Z')
        .or_else(|| text.strip_suffix("+00:00"))?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let time = time.split('.').next()?;
    let mut time = time.split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..61).contains(&second)
    {
        return None;
    }
    // Howard Hinnant's days-from-civil algorithm.
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second)
}
