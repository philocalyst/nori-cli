//! Telling the terminal what Nori is doing: the Program Status Protocol (OSC 7501).
//!
//! This module is the only place that writes the protocol. It owns three things:
//!
//! * [`wire`]: the encoding of a record, with the protocol's limits.
//! * Detection: at startup the terminal is asked whether it understands the protocol
//!   ([`probe`]); when it does not, nothing is ever written. `NORI_PROGRAM_STATUS=off` turns
//!   the reports off, and `NORI_PROGRAM_STATUS=on` sends them without asking.
//! * The outbox: reports are queued here and leave inside the frame writer's next write, so a
//!   report is never interleaved with a frame. When no frame is coming, the app flushes the
//!   outbox itself between events.
//!
//! What to say, and when, is decided by `app::program_status`, which turns the session's events
//! into records. See `docs/terminal-integration.md`.

use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[cfg(unix)]
mod probe;
pub(crate) mod rex_links;
pub(crate) mod wire;

pub(crate) use wire::Capabilities;
pub(crate) use wire::Kind;
pub(crate) use wire::Report;
pub(crate) use wire::State;

/// `None` inside: the terminal does not take reports.
static CAPABILITIES: OnceLock<Option<Capabilities>> = OnceLock::new();
/// Sequences waiting for the next write to the terminal.
static OUTBOX: Mutex<Vec<u8>> = Mutex::new(Vec::new());
static HAS_PENDING: AtomicBool = AtomicBool::new(false);
/// Whether the terminal holds records from this process.
static LIVE: AtomicBool = AtomicBool::new(false);
/// The records were withdrawn because the terminal was handed back (suspend, a child program);
/// the reporter says them again when the screen is ours again.
static RETRACTED: AtomicBool = AtomicBool::new(false);

/// How the user asked for reports through `NORI_PROGRAM_STATUS`.
enum Preference {
    Off,
    On,
    Ask,
}

fn preference() -> Preference {
    match std::env::var("NORI_PROGRAM_STATUS")
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Ok("0" | "off" | "false" | "no" | "never") => Preference::Off,
        Ok("1" | "on" | "true" | "yes" | "always") => Preference::On,
        Ok(_) | Err(_) => Preference::Ask,
    }
}

/// Decide, once, whether this terminal takes reports. Call with the terminal in raw mode and
/// before the event reader exists.
pub(crate) fn init() {
    CAPABILITIES.get_or_init(|| match preference() {
        Preference::Off => None,
        Preference::On => Some(Capabilities::full()),
        Preference::Ask => detect(),
    });
}

fn detect() -> Option<Capabilities> {
    #[cfg(unix)]
    {
        if std::env::var("TERM").is_ok_and(|term| term == "dumb") {
            return None;
        }
        probe::query()
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub(crate) fn capabilities() -> Option<&'static Capabilities> {
    CAPABILITIES.get().and_then(Option::as_ref)
}

fn push(sequence: &str) {
    OUTBOX
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .extend_from_slice(sequence.as_bytes());
    HAS_PENDING.store(true, Ordering::Release);
}

/// Queue a report. Returns whether it was queued: `false` when the terminal does not take
/// reports or cannot take this one.
pub(crate) fn send(report: &Report) -> bool {
    let Some(sequence) = capabilities().and_then(|caps| report.encode(caps)) else {
        return false;
    };
    push(&sequence);
    LIVE.store(true, Ordering::Release);
    true
}

/// Queue the removal of a record and everything beneath it; of every record when `id` is empty.
pub(crate) fn clear(id: &str) {
    if capabilities().is_none() {
        return;
    }
    push(&wire::clear_sequence(id));
    if id.is_empty() {
        LIVE.store(false, Ordering::Release);
    }
}

/// The queued sequences, for the next write to the terminal.
pub(crate) fn take_pending() -> Vec<u8> {
    if !HAS_PENDING.swap(false, Ordering::AcqRel) {
        return Vec::new();
    }
    std::mem::take(&mut *OUTBOX.lock().unwrap_or_else(PoisonError::into_inner))
}

pub(crate) fn has_pending() -> bool {
    HAS_PENDING.load(Ordering::Acquire)
}

/// The terminal is being handed back (suspend, a child program, a panic): send what is queued
/// and withdraw every record now, since nothing here will be running to retract them. The
/// reporter says them again with [`take_retracted`] when it is back.
pub(crate) fn retract_now() {
    use std::io::Write;
    let mut bytes = take_pending();
    let live = LIVE.swap(false, Ordering::AcqRel);
    if live {
        bytes.extend_from_slice(wire::clear_sequence("").as_bytes());
        RETRACTED.store(true, Ordering::Release);
    }
    if !bytes.is_empty() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(&bytes);
        let _ = out.flush();
    }
}

/// Whether records were withdrawn by [`retract_now`] since the last call.
pub(crate) fn take_retracted() -> bool {
    RETRACTED.swap(false, Ordering::AcqRel)
}
