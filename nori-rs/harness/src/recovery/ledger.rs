//! What a conversation has learned about routes that failed it.
//!
//! The ledger is keyed by conversation id, which a provider handoff keeps, so the session that
//! takes over from an exhausted agent never offers that agent (or the route that just failed)
//! again. It lives for the process; a new conversation starts clean.
//!
//! It also remembers the route the user chose (the primary) once a fallback has moved the
//! conversation off it, with when it may be used again, so the harness can move back.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::PoisonError;

use nori_workflow::Exclusion;
use nori_workflow::ReturnToPrimary;
use nori_workflow::RouteRef;

struct Entry {
    exclusion: Exclusion,
    /// When the exclusion lapses, in seconds since the Unix epoch; `None` for the conversation.
    expires_at: Option<i64>,
    /// A route given up on because the session moved off it, not because it is known bad: going
    /// back to the primary route forgives it.
    forgivable: bool,
}

static LEDGER: Mutex<Option<HashMap<String, Vec<Entry>>>> = Mutex::new(None);

/// How the harness learns that the primary route works again, when waiting for a time is not
/// enough.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recheck {
    /// Nothing to check: the time is the whole signal.
    None,
    /// The model failed as unavailable: ask the agent what it offers now (`probes` checks so far).
    /// The agent advertises its models; a stale id vanishes from that list.
    Reprobe { probes: i64 },
}

/// The route the user chose, which a fallback moved the conversation off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Primary {
    pub(crate) route: RouteRef,
    pub(crate) policy: ReturnToPrimary,
    /// When `after_reset` may go back, in seconds since the Unix epoch. `None`: no time is
    /// known, so it goes back when a new session starts, as `next_session` does.
    pub(crate) restore_at: Option<i64>,
    pub(crate) recheck: Recheck,
}

static PRIMARIES: Mutex<Option<HashMap<String, Primary>>> = Mutex::new(None);

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The routes `conversation` must not be offered, with lapsed exclusions dropped.
pub(crate) fn exclusions(conversation: &str) -> Vec<Exclusion> {
    let mut ledger = LEDGER.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(entries) = ledger.get_or_insert_default().get_mut(conversation) else {
        return Vec::new();
    };
    let now = now();
    entries.retain(|entry| entry.expires_at.is_none_or(|at| at > now));
    entries
        .iter()
        .map(|entry| entry.exclusion.clone())
        .collect()
}

pub(crate) fn exclude(
    conversation: &str,
    exclusion: Exclusion,
    expires_at: Option<i64>,
    forgivable: bool,
) {
    let mut ledger = LEDGER.lock().unwrap_or_else(PoisonError::into_inner);
    let entries = ledger
        .get_or_insert_default()
        .entry(conversation.to_owned())
        .or_default();
    entries.retain(|entry| entry.exclusion != exclusion);
    entries.push(Entry {
        exclusion,
        expires_at,
        forgivable,
    });
}

/// Going back to `route`: drop the exclusion that only said the session left it.
pub(crate) fn forgive(conversation: &str, route: &RouteRef) {
    let mut ledger = LEDGER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(entries) = ledger.get_or_insert_default().get_mut(conversation) {
        entries.retain(|entry| {
            !(entry.forgivable && entry.exclusion == Exclusion::Route(route.clone()))
        });
    }
}

/// Remember that the conversation left `route` (the first one it left is the primary).
/// `restore_at` only ever moves later.
pub(crate) fn note_primary(
    conversation: &str,
    route: &RouteRef,
    policy: ReturnToPrimary,
    restore_at: Option<i64>,
    recheck: Recheck,
) {
    if policy == ReturnToPrimary::Never {
        return;
    }
    let mut primaries = PRIMARIES.lock().unwrap_or_else(PoisonError::into_inner);
    let primary = primaries
        .get_or_insert_default()
        .entry(conversation.to_owned())
        .or_insert_with(|| Primary {
            route: route.clone(),
            policy,
            restore_at,
            recheck,
        });
    if restore_at > primary.restore_at {
        primary.restore_at = restore_at;
        primary.recheck = recheck;
    }
}

/// The primary route is not ready yet: look again at `restore_at` (never, when `None`).
pub(crate) fn reschedule(conversation: &str, restore_at: Option<i64>, recheck: Recheck) {
    if let Some(primary) = PRIMARIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .get_mut(conversation)
    {
        primary.restore_at = restore_at;
        primary.recheck = recheck;
    }
}

/// `route` works again: drop every exclusion that named it.
pub(crate) fn pardon(conversation: &str, route: &RouteRef) {
    let mut ledger = LEDGER.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(entries) = ledger.get_or_insert_default().get_mut(conversation) {
        entries.retain(|entry| entry.exclusion != Exclusion::Route(route.clone()));
    }
}

pub(crate) fn primary(conversation: &str) -> Option<Primary> {
    PRIMARIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .get(conversation)
        .cloned()
}

pub(crate) fn clear_primary(conversation: &str) {
    PRIMARIES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_default()
        .remove(conversation);
}

/// Whether an exclusion that does not lapse rules `route` out for this conversation: usage that
/// is exhausted, or a model known to be gone. A route merely left behind does not.
pub(crate) fn blocks(conversation: &str, route: &RouteRef) -> bool {
    let mut ledger = LEDGER.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(entries) = ledger.get_or_insert_default().get(conversation) else {
        return false;
    };
    let now = now();
    entries.iter().any(|entry| {
        !entry.forgivable
            && entry.expires_at.is_none_or(|at| at > now)
            && entry.exclusion.covers(route)
    })
}

/// A shorthand for "now" in seconds since the Unix epoch.
pub(crate) fn now_seconds() -> i64 {
    now()
}

/// When a quota the agent says renews at `resets_at` (RFC 3339 or epoch seconds or
/// milliseconds) is available again, in epoch seconds.
pub(crate) fn parse_reset(resets_at: &str) -> Option<i64> {
    let text = resets_at.trim();
    if let Ok(number) = text.parse::<i64>() {
        return Some(if number > 100_000_000_000 {
            number / 1000
        } else {
            number
        });
    }
    chrono::DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.timestamp())
}
