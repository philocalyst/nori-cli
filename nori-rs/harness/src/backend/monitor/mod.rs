//! Monitors: background watchers an agent arms and then stops thinking about.
//!
//! An agent that waits (for a build, CI, a dev server, a file, a peer) has two bad options: hold
//! its turn, or poll. A monitor is the third: the agent arms a watcher with `monitor_start`, ends
//! its turn, and each event the watcher produces reaches it as a new message, through the same
//! turn ledger as every other prompt nobody typed.
//!
//! The pieces, each in one place:
//!
//! - [`spec`]: what a monitor is (source, rules, limits). Journaled, so a resume can restart it.
//! - [`source`]: one task per source (command, file, watch, actor, ci, http_poll), feeding lines.
//! - [`pipeline`]: lines to events: `fail_on`, `filter`, `until`, batching, flood protection.
//! - [`registry`], [`report`], [`lease`], [`view`], [`restore`]: [`Monitors`], the one owner of
//!   monitors and of the queue of events nobody has been handed.
//! - [`delivery`]: the backend's side: offering a batch to the ledger when the agent is idle.
//! - [`journal`], [`text`]: the journal records, and what the agent reads.
//!
//! The invariants, in one list:
//!
//! 1. An event is journaled before it can be queued, queued once, leased once, delivered once. A
//!    tool result that carries events (`monitor_list`, `monitor_stop`) consumes them.
//! 2. A running turn is never interrupted. Events wait for the agent to be idle; a failure or a
//!    monitor's end wakes it, and so does an ordinary event of an `immediate` monitor. A
//!    `boundary` monitor's events come with the next ordinary turn's end; a `digest` monitor's
//!    after `digest_seconds`.
//! 3. A monitor says how it ended: its exit code, that it expired (with how many events), that
//!    it flooded. Nothing reports success that was not observed.
//! 4. A monitor lives with its actor's session and ends with it; the journal keeps it armed, so
//!    a resume restores it dormant, with its unseen events, and the agent is told.

mod delivery;
mod event;
mod journal;
mod lease;
mod pipeline;
mod registry;
mod report;
mod request;
mod restore;
pub(crate) mod source;
pub(crate) mod spec;
mod text;
mod view;

pub(crate) use event::Ending;
pub(crate) use registry::Monitors;
pub(crate) use registry::Overrides;
pub(crate) use request::MonitorRearmRequest;
pub(crate) use request::MonitorStartRequest;
pub(crate) use request::MonitorTargetRequest;
pub(crate) use text::events_json;
