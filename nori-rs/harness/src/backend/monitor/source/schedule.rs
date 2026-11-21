//! The `timer` source: a period, an instant, or a cron schedule, each firing an event.
//!
//! A period counts on the runtime's clock, so a test on a paused clock fires it exactly. An instant
//! and a cron schedule are wall-clock times in the machine's time zone; a schedule time that a
//! daylight-saving change skips does not fire that day.

use std::time::Duration;

use chrono::DateTime;
use chrono::Datelike;
use chrono::Local;
use chrono::NaiveDate;
use chrono::NaiveDateTime;
use chrono::NaiveTime;
use chrono::TimeZone;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc::Sender;

use super::Item;
use crate::backend::monitor::event::Ending;

/// When a timer fires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case")]
pub(crate) enum Schedule {
    /// Every `seconds`, the first time `seconds` after it is armed.
    Every { seconds: i64 },
    /// Once: an RFC 3339 instant, or a local `HH:MM` (today, or tomorrow when it has passed).
    At { at: String },
    /// A cron expression: five fields (minute hour day-of-month month day-of-week), six with
    /// seconds first, or `@hourly`, `@daily`, `@weekly`, `@monthly`, `@yearly`.
    Cron { expression: String },
}

impl Schedule {
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::Every { seconds } => format!("every {seconds}s"),
            Self::At { at } => format!("at {at}"),
            Self::Cron { expression } => format!("cron {expression}"),
        }
    }

    /// Check the schedule and fix what it means now: the error says what to write instead.
    pub(crate) fn compile(&self, now: DateTime<Local>) -> Result<Compiled, String> {
        match self {
            Self::Every { seconds } if *seconds < 1 => {
                Err("`every_seconds` must be at least 1".to_owned())
            }
            Self::Every { seconds } => {
                Ok(Compiled::Every(Duration::from_secs(seconds.unsigned_abs())))
            }
            Self::At { at } => at_instant(at, now).map(Compiled::At),
            Self::Cron { expression } => Cron::parse(expression).map(Compiled::Cron),
        }
    }
}

/// A schedule ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Compiled {
    Every(Duration),
    At(DateTime<Local>),
    Cron(Cron),
}

fn at_instant(at: &str, now: DateTime<Local>) -> Result<DateTime<Local>, String> {
    let at = at.trim();
    if let Ok(instant) = DateTime::parse_from_rfc3339(at) {
        return Ok(instant.with_timezone(&Local));
    }
    let time = NaiveTime::parse_from_str(at, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(at, "%H:%M:%S"))
        .map_err(|_| {
            format!(
                "`at` {at:?} is neither an RFC 3339 instant (2026-11-20T09:00:00Z) nor a local time (09:00)"
            )
        })?;
    let mut day = now.date_naive();
    for _ in 0..3 {
        if let Some(instant) = Local
            .from_local_datetime(&day.and_time(time))
            .earliest()
            .filter(|instant| *instant > now)
        {
            return Ok(instant);
        }
        day = day
            .succ_opt()
            .ok_or("`at` is past the end of the calendar")?;
    }
    Err(format!("`at` {at:?} does not occur in the next days"))
}

/// A parsed cron expression. Each field is a set of allowed values as bits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Cron {
    seconds: i64,
    minutes: i64,
    hours: i64,
    days: i64,
    months: i64,
    weekdays: i64,
    /// Day-of-month and day-of-week were both restricted: either may match, as cron does.
    either_day: bool,
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const WEEKDAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

impl Cron {
    pub(crate) fn parse(expression: &str) -> Result<Self, String> {
        let expanded = match expression.trim() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            other => other,
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let (seconds, rest) = match fields.len() {
            5 => ("0", &fields[..]),
            6 => (fields[0], &fields[1..]),
            count => {
                return Err(format!(
                    "cron {expression:?} has {count} fields; write five (minute hour day month weekday) or six with seconds first"
                ));
            }
        };
        let field = |name: &str, text: &str, low: i64, high: i64, names: &[&str]| {
            parse_field(text, low, high, names)
                .map_err(|problem| format!("cron {expression:?}, {name} field {text:?}: {problem}"))
        };
        let weekdays = field("weekday", rest[4], 0, 7, &WEEKDAYS)?;
        // 7 is Sunday too.
        let weekdays = if weekdays & (1 << 7) != 0 {
            (weekdays | 1) & !(1 << 7)
        } else {
            weekdays
        };
        Ok(Self {
            seconds: field("second", seconds, 0, 59, &[])?,
            minutes: field("minute", rest[0], 0, 59, &[])?,
            hours: field("hour", rest[1], 0, 23, &[])?,
            days: field("day-of-month", rest[2], 1, 31, &[])?,
            months: field("month", rest[3], 1, 12, &MONTHS)?,
            weekdays,
            either_day: rest[2] != "*" && rest[4] != "*",
        })
    }

    fn day_matches(&self, date: NaiveDate) -> bool {
        let month = has(self.months, i64::from(date.month()));
        let day = has(self.days, i64::from(date.day()));
        let weekday = has(
            self.weekdays,
            i64::from(date.weekday().num_days_from_sunday()),
        );
        month
            && if self.either_day {
                day || weekday
            } else {
                day && weekday
            }
    }

    /// The first firing strictly after `after`, within the next eight years.
    pub(crate) fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let start = after.naive_local();
        let mut date = start.date();
        for _ in 0..(366 * 8) {
            if self.day_matches(date) {
                for hour in bits(self.hours) {
                    for minute in bits(self.minutes) {
                        for second in bits(self.seconds) {
                            let Some(time) = NaiveTime::from_hms_opt(
                                u32::try_from(hour).ok()?,
                                u32::try_from(minute).ok()?,
                                u32::try_from(second).ok()?,
                            ) else {
                                continue;
                            };
                            let candidate = NaiveDateTime::new(date, time);
                            if candidate <= start {
                                continue;
                            }
                            if let Some(instant) = Local.from_local_datetime(&candidate).earliest()
                                && instant > after
                            {
                                return Some(instant);
                            }
                        }
                    }
                }
            }
            date = date.succ_opt()?;
        }
        None
    }
}

fn has(set: i64, value: i64) -> bool {
    (0..63).contains(&value) && set & (1 << value) != 0
}

fn bits(set: i64) -> impl Iterator<Item = i64> {
    (0..63).filter(move |value| set & (1 << value) != 0)
}

fn parse_value(text: &str, low: i64, high: i64, names: &[&str]) -> Result<i64, String> {
    let lower = text.to_ascii_lowercase();
    let value = match names.iter().position(|name| *name == lower) {
        // Month names count from 1, weekday names from 0.
        Some(index) => i64::try_from(index).unwrap_or(0) + if names.len() == 12 { 1 } else { 0 },
        None => text
            .parse::<i64>()
            .map_err(|_| format!("{text:?} is not a number"))?,
    };
    if (low..=high).contains(&value) {
        Ok(value)
    } else {
        Err(format!("{value} is outside {low}-{high}"))
    }
}

fn parse_field(text: &str, low: i64, high: i64, names: &[&str]) -> Result<i64, String> {
    let mut set = 0;
    for part in text.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => (
                range,
                step.parse::<i64>()
                    .ok()
                    .filter(|step| *step > 0)
                    .ok_or_else(|| format!("step {step:?} is not a positive number"))?,
            ),
            None => (part, 1),
        };
        let (first, last) = match range {
            "*" => (low, high),
            range => match range.split_once('-') {
                Some((first, last)) => (
                    parse_value(first, low, high, names)?,
                    parse_value(last, low, high, names)?,
                ),
                // `5/15` means from 5 to the end, every 15.
                None if part.contains('/') => (parse_value(range, low, high, names)?, high),
                None => {
                    let value = parse_value(range, low, high, names)?;
                    (value, value)
                }
            },
        };
        if first > last {
            return Err(format!("the range {first}-{last} runs backwards"));
        }
        let mut value = first;
        while value <= last {
            set |= 1 << value;
            let Some(next) = value.checked_add(step) else {
                break;
            };
            value = next;
        }
    }
    Ok(set)
}

/// Fire until the schedule has no more firings; each firing is a line.
pub(super) async fn run(schedule: Compiled, tx: &Sender<Item>) -> Option<Ending> {
    let mut fired: i64 = 0;
    let fire = |fired: i64, at: DateTime<Local>| Item::Line {
        text: format!("fired ({fired}) at {}", at.format("%Y-%m-%d %H:%M:%S %Z")),
        failure: false,
    };
    match schedule {
        Compiled::Every(period) => {
            let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticks.tick().await;
                fired += 1;
                if tx.send(fire(fired, Local::now())).await.is_err() {
                    return None;
                }
            }
        }
        Compiled::At(at) => {
            sleep_until(at).await;
            if tx.send(fire(1, at)).await.is_err() {
                return None;
            }
            Some(Ending::Finished {
                ok: true,
                note: format!("The time came ({}).", at.format("%Y-%m-%d %H:%M %Z")),
            })
        }
        Compiled::Cron(cron) => loop {
            let Some(next) = cron.next_after(Local::now()) else {
                return Some(Ending::Finished {
                    ok: true,
                    note: format!("The schedule has no more times, after {fired} firing(s)."),
                });
            };
            sleep_until(next).await;
            fired += 1;
            if tx.send(fire(fired, next)).await.is_err() {
                return None;
            }
        },
    }
}

/// Sleep until a wall-clock instant, re-reading the clock so a machine that slept wakes on time.
async fn sleep_until(at: DateTime<Local>) {
    const SLICE: Duration = Duration::from_secs(60);
    loop {
        let left = (at - Local::now()).to_std().unwrap_or(Duration::ZERO);
        if left.is_zero() {
            return;
        }
        tokio::time::sleep(left.min(SLICE)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn local(text: &str) -> DateTime<Local> {
        let naive = NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").expect("a time");
        Local
            .from_local_datetime(&naive)
            .earliest()
            .expect("a local time")
    }

    fn next(expression: &str, after: &str) -> String {
        Cron::parse(expression)
            .expect("a cron expression")
            .next_after(local(after))
            .expect("a next time")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    #[test]
    fn cron_fields_steps_ranges_names_and_seconds() {
        assert_eq!(
            next("*/15 * * * *", "2026-11-20 10:07:30"),
            "2026-11-20 10:15:00"
        );
        assert_eq!(
            next("0 3 * * 1-5", "2026-11-20 10:00:00"),
            "2026-11-23 03:00:00"
        );
        assert_eq!(
            next("30 9 1 jan *", "2026-11-20 10:00:00"),
            "2027-01-01 09:30:00"
        );
        assert_eq!(
            next("*/10 * * * * *", "2026-11-20 10:00:05"),
            "2026-11-20 10:00:10"
        );
        assert_eq!(next("@daily", "2026-11-20 10:00:00"), "2026-11-21 00:00:00");
        // Both day fields restricted: either matches (the 1st, or any Friday).
        assert_eq!(
            next("0 0 1 * fri", "2026-11-20 10:00:00"),
            "2026-11-27 00:00:00"
        );
        assert_eq!(
            next("0 0 * * 7", "2026-11-20 10:00:00"),
            "2026-11-22 00:00:00"
        );
    }

    #[test]
    fn a_bad_cron_says_which_field() {
        assert_eq!(
            Cron::parse("61 * * * *"),
            Err("cron \"61 * * * *\", minute field \"61\": 61 is outside 0-59".to_owned())
        );
        assert!(Cron::parse("* * *").is_err());
    }

    #[test]
    fn a_very_large_step_does_not_overflow() {
        assert_eq!(parse_field("0/9223372036854775807", 0, 59, &[]), Ok(1));
    }

    #[test]
    fn at_a_local_time_is_the_next_one() {
        let now = local("2026-11-20 10:00:00");
        let Compiled::At(at) = (Schedule::At {
            at: "09:30".to_owned(),
        })
        .compile(now)
        .expect("compiles") else {
            panic!("not an instant");
        };
        assert_eq!(
            at.format("%Y-%m-%d %H:%M").to_string(),
            "2026-11-21 09:30".to_owned()
        );
    }
}
