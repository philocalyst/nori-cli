//! Gates: Pkl-declared rules consulted synchronously on the hot path ([docs/pkl-runtime.md]).
//!
//! Monitors observe and act afterwards; a gate decides first. A `nori:Gate` module is evaluated
//! with the other modules when the session starts, and [`Gates::compile`] turns every gate of the
//! catalog into matchers once (regular expressions and globs built, rules in order). After that a
//! check is a plain function over a few strings: it never evaluates Pkl, spawns nothing, holds no
//! lock and allocates only for the facts it reads out of the request. The compiled set lives in a
//! [`GateCell`] shared by the whole delegation tree, so a child's permission requests and tool
//! calls meet the same gates as its parent's.
//!
//! Three events are gated:
//!
//! - `permission` ([`permission`]): `session/request_permission`, in `handle_permission_request`.
//! - `host_tool` ([`host_tool`]): a call to one of Nori's own tools, both the in-process dispatch
//!   and the `nori-client` MCP server.
//! - `stop` ([`stop`]): a local turn ended with `end_turn`, in `handle_completed_turn`.
//!
//! The first rule that matches decides; no match is today's behaviour. Every decision is journaled
//! as a `gate` command ([`record`]) and every refusal, rewrite and continuation (and an approval
//! the person would otherwise have given) is one quiet line.
//!
//! [docs/pkl-runtime.md]: ../../../../docs/pkl-runtime.md

mod call_facts;
mod cell;
mod compile;
mod host_tool;
mod permission;
mod record;
mod stop;
#[cfg(test)]
mod tests;

pub(crate) use cell::GateCell;
pub(crate) use host_tool::gate_host_call;
pub(in crate::backend) use permission::Gated;

use regex::Regex;

use host_tool::HostTool;
use nori_pkl::kinds::gate::Pattern;
use permission::Permission;
use stop::Stop;

/// A regular expression a text must match, or must not.
#[derive(Debug)]
struct TextMatcher {
    regex: Regex,
    wanted: bool,
}

impl TextMatcher {
    fn compile(field: &str, pattern: &Pattern) -> Result<Self, String> {
        let (source, wanted) = pattern.parts();
        Regex::new(source)
            .map(|regex| Self { regex, wanted })
            .map_err(|error| format!("{field}: {source:?} does not compile: {error}"))
    }

    fn matches(&self, text: &str) -> bool {
        self.regex.is_match(text) == self.wanted
    }
}

/// A rule with the gate it belongs to and its place in that gate.
#[derive(Debug)]
struct Entry<R> {
    gate: String,
    index: i64,
    rule: R,
}

/// The rule that decided, and whose it is.
#[derive(Debug)]
pub(crate) struct Hit<'a, R> {
    /// The gate's name.
    pub(crate) gate: &'a str,
    /// The rule's position in the gate, from zero.
    pub(crate) index: i64,
    pub(crate) rule: &'a R,
}

impl<'a, R> Hit<'a, R> {
    fn of(entry: &'a Entry<R>) -> Self {
        Self {
            gate: &entry.gate,
            index: entry.index,
            rule: &entry.rule,
        }
    }
}

/// Every gate of the catalog, compiled. Gates are consulted in the catalog's order (by name),
/// each gate's rules in theirs.
#[derive(Debug, Default)]
pub(crate) struct Gates {
    permission: Vec<Entry<Permission>>,
    host_tool: Vec<Entry<HostTool>>,
    stop: Vec<Entry<Stop>>,
}

impl Gates {
    /// Whether any gate listens to `permission`.
    pub(crate) fn has_permission(&self) -> bool {
        !self.permission.is_empty()
    }

    /// Whether any gate listens to `host_tool`.
    pub(crate) fn has_host_tool(&self) -> bool {
        !self.host_tool.is_empty()
    }

    /// Whether any gate listens to `stop`.
    pub(crate) fn has_stop(&self) -> bool {
        !self.stop.is_empty()
    }
}
