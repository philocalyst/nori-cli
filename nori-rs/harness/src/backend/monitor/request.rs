//! The arguments of `monitor_start`, and how they become a [`Spec`].
//!
//! The schema is flat, because that is what models fill in reliably: a `source` and the fields
//! that source reads. Turning it into a [`Spec`] is where a missing field, or a field that
//! belongs to another source, becomes an error that says what to write instead.

use std::path::Path;

use rmcp::schemars;
use serde::Deserialize;

use super::source::Schedule;
use super::source::resolve_path;
use super::spec::Action;
use super::spec::ChangeKind;
use super::spec::Extract;
use super::spec::Limits;
use super::spec::MAX_EVENTS_CEILING;
use super::spec::MAX_TIMEOUT_SECONDS;
use super::spec::Rules;
use super::spec::SessionTrigger;
use super::spec::Source;
use super::spec::Spec;
use super::spec::Track;
use super::spec::WakeMode;

/// A list, or the one string a model wrote instead of a list.
#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(untagged)]
pub(crate) enum Strings {
    Many(Vec<String>),
    One(String),
}

impl Strings {
    fn into_vec(self) -> Vec<String> {
        match self {
            Self::Many(many) => many,
            Self::One(one) => vec![one],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(crate) enum SourceKind {
    Command,
    File,
    Watch,
    Actor,
    Ci,
    HttpPoll,
    Timer,
    Session,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct MonitorStartRequest {
    /// Required. A short name, such as `tests` or `ci`: it heads the events and addresses the
    /// monitor in `monitor_stop` and `monitor_rearm`. Unique among your running monitors.
    label: String,
    /// Required. What to watch. Set only the fields of that source (marked below).
    source: SourceKind,
    /// `command`: the shell command. Each stdout line is an event; when it exits, the exit code
    /// is the last event and non-zero is a failure.
    #[serde(default)]
    command: Option<String>,
    /// `command`: the directory to run in. Defaults to your shell's current directory.
    #[serde(default)]
    cwd: Option<String>,
    /// `command`: also turn stderr lines into events. Without it stderr is not streamed, but its
    /// last lines come with a non-zero exit.
    #[serde(default)]
    stderr: Option<bool>,
    /// `file`: the file to tail. It must exist.
    #[serde(default)]
    path: Option<String>,
    /// `file`: report the lines already in the file too, not only new ones.
    #[serde(default)]
    from_start: Option<bool>,
    /// `watch`: files or directories (watched recursively; `.git`, `node_modules` and `target`
    /// below them are skipped).
    #[serde(default)]
    paths: Option<Strings>,
    /// `watch`: which changes are events: `created`, `modified`, `deleted`. Default: all.
    #[serde(default)]
    kinds: Option<Vec<ChangeKind>>,
    /// `actor`: the peer to watch: an alias, `actor://alias` or a session id.
    #[serde(default)]
    actor: Option<String>,
    /// `actor`: what to report: `state`, `results`, `replies`. Default: all.
    #[serde(default)]
    track: Option<Vec<Track>>,
    /// `ci`: the commit to watch. Default: the commit the branch was last pushed at.
    #[serde(default)]
    sha: Option<String>,
    /// `ci`: the branch. Default: the checked-out one.
    #[serde(default)]
    branch: Option<String>,
    /// `http_poll`: the URL to GET. No credentials: use `command` with curl for those.
    #[serde(default)]
    url: Option<String>,
    /// `http_poll`: seconds between reads (default 30, at least 2).
    #[serde(default)]
    interval_seconds: Option<i64>,
    /// `http_poll`: what to pick from a JSON response, such as `$.status` or `$.items[0].state`.
    /// An event only when it changes. With neither this nor `regex`, the HTTP status is watched.
    #[serde(default)]
    json_path: Option<String>,
    /// `http_poll`: a regular expression for the response text; its first group (else the whole
    /// match) is the value.
    #[serde(default)]
    regex: Option<String>,
    /// `timer`: fire every this many seconds (the first time that long from now).
    #[serde(default)]
    every_seconds: Option<i64>,
    /// `timer`: fire once at this time: RFC 3339 (`2026-11-20T09:00:00Z`) or a local `HH:MM`
    /// (the next one).
    #[serde(default)]
    at: Option<String>,
    /// `timer`: fire on this cron schedule, local time: five fields (`0 9 * * 1-5` is 09:00 on
    /// weekdays), six with seconds first, or `@hourly`/`@daily`/`@weekly`.
    #[serde(default)]
    cron: Option<String>,
    /// `session`: which moments are events: `turn_ended` (not the turns your own wakes start),
    /// `turn_failed`, `route_changed`, `subagent_ended`, `permission_requested`, `quota_low`.
    #[serde(default)]
    events: Option<Vec<SessionTrigger>>,
    /// `session`: an event when the session has been idle this many seconds (once per idle
    /// stretch).
    #[serde(default)]
    idle_seconds: Option<i64>,
    /// What an event does, each by `kind`. Default: `wake` you with it. `notify` tells the person
    /// instead and does not wake you. Templates `{{event}}`, `{{label}}` and `{{monitor}}` are
    /// filled in.
    #[serde(default)]
    actions: Option<Vec<Action>>,
    /// Only lines that match this regular expression are events.
    #[serde(default)]
    filter: Option<String>,
    /// Regular expressions for failure signatures. A line that matches is always an event,
    /// marked FAILED, whatever `filter` says, and wakes you at once.
    #[serde(default)]
    fail_on: Option<Strings>,
    /// A regular expression: the monitor ends after emitting the first line that matches.
    #[serde(default)]
    until: Option<String>,
    /// Lines that arrive within this many milliseconds of each other are one event. Defaults
    /// to 300 (600 for `watch`).
    #[serde(default)]
    batch_ms: Option<i64>,
    /// The monitor stops, and says so, after this many events (default 50, at most 500).
    #[serde(default)]
    max_events: Option<i64>,
    /// The monitor ends, reporting that it expired, after this long. Default 1800 (45 minutes for
    /// `ci`, an hour for `file` and `watch`); at most 28800.
    #[serde(default)]
    timeout_seconds: Option<i64>,
    /// When events wake you while you are idle: `immediate` (default), `digest` (one wake per
    /// `digest_seconds`), or `boundary` (never by themselves). Failures and the monitor's end
    /// always wake you.
    #[serde(default)]
    wake: Option<WakeMode>,
    /// With `wake: digest`, how long events are gathered (default 30).
    #[serde(default)]
    digest_seconds: Option<i64>,
}

impl MonitorStartRequest {
    /// The spec this asks for, with paths resolved against `cwd`.
    pub(crate) fn into_spec(self, cwd: &Path) -> Result<Spec, String> {
        let kind = self.source;
        let name = match kind {
            SourceKind::Command => "command",
            SourceKind::File => "file",
            SourceKind::Watch => "watch",
            SourceKind::Actor => "actor",
            SourceKind::Ci => "ci",
            SourceKind::HttpPoll => "http_poll",
            SourceKind::Timer => "timer",
            SourceKind::Session => "session",
        };
        // Fields of other sources would be silently ignored: say so instead.
        let strays: Vec<&str> = [
            ("command", self.command.is_some(), SourceKind::Command),
            ("cwd", self.cwd.is_some(), SourceKind::Command),
            ("stderr", self.stderr.is_some(), SourceKind::Command),
            ("path", self.path.is_some(), SourceKind::File),
            ("from_start", self.from_start.is_some(), SourceKind::File),
            ("paths", self.paths.is_some(), SourceKind::Watch),
            ("kinds", self.kinds.is_some(), SourceKind::Watch),
            ("actor", self.actor.is_some(), SourceKind::Actor),
            ("track", self.track.is_some(), SourceKind::Actor),
            ("sha", self.sha.is_some(), SourceKind::Ci),
            ("branch", self.branch.is_some(), SourceKind::Ci),
            ("url", self.url.is_some(), SourceKind::HttpPoll),
            (
                "interval_seconds",
                self.interval_seconds.is_some(),
                SourceKind::HttpPoll,
            ),
            ("json_path", self.json_path.is_some(), SourceKind::HttpPoll),
            ("regex", self.regex.is_some(), SourceKind::HttpPoll),
            (
                "every_seconds",
                self.every_seconds.is_some(),
                SourceKind::Timer,
            ),
            ("at", self.at.is_some(), SourceKind::Timer),
            ("cron", self.cron.is_some(), SourceKind::Timer),
            ("events", self.events.is_some(), SourceKind::Session),
            (
                "idle_seconds",
                self.idle_seconds.is_some(),
                SourceKind::Session,
            ),
        ]
        .into_iter()
        .filter(|(_, present, owner)| *present && *owner != kind)
        .map(|(field, _, _)| field)
        .collect();
        if !strays.is_empty() {
            return Err(format!(
                "{} do(es) not belong to `source: {name}`. Remove {}, or change `source`.",
                strays.join(", "),
                if strays.len() == 1 { "it" } else { "them" }
            ));
        }
        let label = self.label.trim().to_owned();
        if label.is_empty() || label.chars().count() > 40 {
            return Err("`label` must be a short name, 1 to 40 characters".to_owned());
        }
        let need = |value: Option<String>, field: &str| {
            value
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| format!("`source: {name}` needs `{field}`"))
        };
        let source = match kind {
            SourceKind::Command => Source::Command {
                command: need(self.command, "command")?,
                cwd: self
                    .cwd
                    .map_or_else(|| cwd.to_path_buf(), |dir| resolve_path(cwd, &dir)),
                stderr: self.stderr.unwrap_or(false),
            },
            SourceKind::File => Source::File {
                path: resolve_path(cwd, &need(self.path, "path")?),
                from_start: self.from_start.unwrap_or(false),
            },
            SourceKind::Watch => {
                let paths: Vec<_> = self
                    .paths
                    .map(Strings::into_vec)
                    .unwrap_or_default()
                    .iter()
                    .map(|path| resolve_path(cwd, path))
                    .collect();
                if paths.is_empty() {
                    return Err("`source: watch` needs `paths`".to_owned());
                }
                let kinds = self.kinds.unwrap_or_else(|| ChangeKind::ALL.to_vec());
                if kinds.is_empty() {
                    return Err("`kinds` is empty: nothing would be reported".to_owned());
                }
                Source::Watch { paths, kinds }
            }
            SourceKind::Actor => {
                let track = self.track.unwrap_or_else(|| Track::ALL.to_vec());
                if track.is_empty() {
                    return Err("`track` is empty: nothing would be reported".to_owned());
                }
                Source::Actor {
                    actor: need(self.actor, "actor")?,
                    track,
                }
            }
            SourceKind::Ci => Source::Ci {
                sha: self.sha,
                branch: self.branch,
                follow_up: None,
            },
            SourceKind::HttpPoll => {
                let extract = match (self.json_path, self.regex) {
                    (Some(_), Some(_)) => {
                        return Err("give `json_path` or `regex`, not both".to_owned());
                    }
                    (Some(path), None) => Extract::JsonPath(path),
                    (None, Some(pattern)) => Extract::Regex(pattern),
                    (None, None) => Extract::Status,
                };
                Source::HttpPoll {
                    url: need(self.url, "url")?,
                    interval_seconds: self.interval_seconds.unwrap_or(30).clamp(2, 3600),
                    extract,
                }
            }
            SourceKind::Timer => Source::Timer {
                schedule: match (self.every_seconds, self.at, self.cron) {
                    (Some(seconds), None, None) => Schedule::Every { seconds },
                    (None, Some(at), None) => Schedule::At { at },
                    (None, None, Some(expression)) => Schedule::Cron { expression },
                    _ => {
                        return Err(
                            "`source: timer` needs exactly one of `every_seconds`, `at` and `cron`"
                                .to_owned(),
                        );
                    }
                },
            },
            SourceKind::Session => Source::Session {
                events: self.events.unwrap_or_default(),
                idle_seconds: self.idle_seconds,
            },
        };
        let positive = |value: Option<i64>, field: &str, default: i64, ceiling: i64| match value {
            Some(value) if value < 1 => Err(format!("`{field}` must be at least 1")),
            Some(value) => Ok(value.min(ceiling)),
            None => Ok(default),
        };
        let limits = Limits {
            timeout_seconds: positive(
                self.timeout_seconds,
                "timeout_seconds",
                source.default_timeout_seconds(),
                MAX_TIMEOUT_SECONDS,
            )?,
            max_events: positive(self.max_events, "max_events", 50, MAX_EVENTS_CEILING)?,
            batch_ms: self
                .batch_ms
                .unwrap_or_else(|| source.default_batch_ms())
                .clamp(0, 60_000),
            digest_seconds: positive(self.digest_seconds, "digest_seconds", 30, 3600)?,
        };
        Ok(Spec {
            label,
            source,
            rules: Rules {
                filter: self.filter,
                fail_on: self.fail_on.map(Strings::into_vec).unwrap_or_default(),
                until: self.until,
            },
            wake: self.wake.unwrap_or_default(),
            limits,
            actions: self.actions.unwrap_or_default(),
            origin: super::spec::Origin::Agent,
        })
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct MonitorTargetRequest {
    /// Required. The monitor's id (`mon-1`) or label; `all` for every running monitor (stop only).
    #[serde(alias = "id", alias = "label", alias = "target")]
    pub(crate) monitor: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct MonitorRearmRequest {
    /// Required. The monitor's id (`mon-1`) or label.
    #[serde(alias = "id", alias = "label", alias = "target")]
    pub(crate) monitor: String,
    /// A new timeout for this run, in seconds.
    #[serde(default)]
    pub(crate) timeout_seconds: Option<i64>,
    /// A new event limit for this run.
    #[serde(default)]
    pub(crate) max_events: Option<i64>,
    /// A new wake mode.
    #[serde(default)]
    pub(crate) wake: Option<WakeMode>,
}
