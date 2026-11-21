//! Standing monitors: the ones a `nori:Monitor` module declares, armed for every session it
//! applies to, as opposed to those an agent arms with `monitor_start`.
//!
//! A module is the same value an agent's request is: one [`Spec`] per trigger, with the module's
//! rules, wake mode and actions. A standing monitor does not count against the agent's own limit
//! of running monitors, never expires unless the module says so, and fires as often as the module
//! allows.

use std::path::Path;

use nori_pkl::catalog::Catalog;
use nori_pkl::kinds::JobEvent;
use nori_pkl::kinds::MonitorDef;
use nori_pkl::kinds::Trigger;

use super::source::Schedule;
use super::source::resolve_path;
use super::spec::Action;
use super::spec::ChangeKind;
use super::spec::Extract;
use super::spec::Limits;
use super::spec::Origin;
use super::spec::Rules;
use super::spec::SessionTrigger;
use super::spec::Source;
use super::spec::Spec;
use super::spec::Track;
use super::spec::WakeMode;

/// How long a standing monitor runs when its module sets no `expires`: as long as a session does.
const STANDING_SECONDS: i64 = 30 * 24 * 3600;

/// What a monitor module arms: monitors now, and watches on job modules for when they run.
#[derive(Debug, Default)]
pub(crate) struct Standing {
    pub(crate) armed: Vec<Spec>,
    pub(crate) on_jobs: Vec<crate::backend::jobs::DeclaredWatch>,
}

/// What `def` (declared in `path`) arms, one monitor per trigger, with paths resolved against
/// `cwd`. A `job` trigger names a job module of `catalog`.
pub(crate) fn specs(
    def: &MonitorDef,
    path: &Path,
    cwd: &Path,
    catalog: &Catalog,
) -> Result<Standing, String> {
    let actions: Vec<Action> = def
        .actions
        .iter()
        .map(|action| match action {
            nori_pkl::kinds::Action::Wake { message } => Action::Wake {
                message: message.clone(),
            },
            nori_pkl::kinds::Action::Prompt { text } => Action::Prompt { text: text.clone() },
            nori_pkl::kinds::Action::Notify { title, body } => Action::Notify {
                title: title.clone(),
                body: body.clone(),
            },
            nori_pkl::kinds::Action::RunJob { job } => Action::RunJob { job: job.clone() },
            nori_pkl::kinds::Action::StopJob { job } => Action::StopJob { job: job.clone() },
            nori_pkl::kinds::Action::RunSkill { skill, arguments } => Action::RunSkill {
                skill: skill.clone(),
                arguments: arguments.clone(),
            },
        })
        .collect();
    let wake = match def.wake {
        nori_pkl::kinds::WakeMode::Immediate => WakeMode::Immediate,
        nori_pkl::kinds::WakeMode::Digest => WakeMode::Digest,
        nori_pkl::kinds::WakeMode::Boundary => WakeMode::Boundary,
    };
    let several = def.on.len() > 1;
    let mut standing = Standing::default();
    for (index, trigger) in def.on.iter().enumerate() {
        let (source, watch) = match trigger {
            Trigger::Job { job, on } => {
                let module = catalog
                    .job(job)
                    .ok_or_else(|| format!("there is no job module named `{job}`"))?;
                let ready = on
                    .contains(&JobEvent::Ready)
                    .then(|| {
                        module
                            .value
                            .ready
                            .as_ref()
                            .map(|ready| ready.pattern.clone())
                    })
                    .flatten();
                // Filled in for each run of the job; this stands for it until then.
                let source = Source::Job {
                    job: String::new(),
                    label: job.clone(),
                    lines: on.contains(&JobEvent::Output),
                    ready: ready.clone(),
                };
                (
                    source,
                    Some((job.clone(), on.contains(&JobEvent::Output), ready)),
                )
            }
            other => (source(other, cwd)?, None),
        };
        let spec = {
            let label = if several {
                format!("{}:{}", def.name, index + 1)
            } else {
                def.name.clone()
            };
            let seconds = |duration: std::time::Duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX).max(1)
            };
            Spec {
                label,
                rules: Rules {
                    filter: def.filter.clone(),
                    fail_on: def.fail_on.clone(),
                    until: def.until.clone(),
                },
                wake,
                limits: Limits {
                    timeout_seconds: def.expires.map_or(STANDING_SECONDS, seconds),
                    max_events: def.max_fires.unwrap_or(i64::MAX),
                    batch_ms: def.debounce.map_or_else(
                        || source.default_batch_ms(),
                        |debounce| i64::try_from(debounce.as_millis()).unwrap_or(i64::MAX),
                    ),
                    digest_seconds: seconds(def.digest),
                },
                actions: actions.clone(),
                origin: Origin::Module {
                    path: path.to_path_buf(),
                },
                source,
            }
        };
        match watch {
            Some((job, lines, ready)) => {
                standing.on_jobs.push(crate::backend::jobs::DeclaredWatch {
                    job,
                    spec,
                    lines,
                    ready,
                });
            }
            None => standing.armed.push(spec),
        }
    }
    Ok(standing)
}

fn source(trigger: &Trigger, cwd: &Path) -> Result<Source, String> {
    let seconds = |duration: std::time::Duration| {
        i64::try_from(duration.as_secs()).unwrap_or(i64::MAX).max(1)
    };
    Ok(match trigger {
        Trigger::Every { interval } => Source::Timer {
            schedule: Schedule::Every {
                seconds: seconds(*interval),
            },
        },
        Trigger::At { time } => Source::Timer {
            schedule: Schedule::At { at: time.clone() },
        },
        Trigger::Cron { expression } => Source::Timer {
            schedule: Schedule::Cron {
                expression: expression.clone(),
            },
        },
        Trigger::Idle { after } => Source::Session {
            events: Vec::new(),
            idle_seconds: Some(seconds(*after)),
        },
        Trigger::Session { events } => Source::Session {
            events: events
                .iter()
                .map(|event| match event {
                    nori_pkl::kinds::SessionEvent::TurnEnded => SessionTrigger::TurnEnded,
                    nori_pkl::kinds::SessionEvent::TurnFailed => SessionTrigger::TurnFailed,
                    nori_pkl::kinds::SessionEvent::RouteChanged => SessionTrigger::RouteChanged,
                    nori_pkl::kinds::SessionEvent::SubagentEnded => SessionTrigger::SubagentEnded,
                    nori_pkl::kinds::SessionEvent::PermissionRequested => {
                        SessionTrigger::PermissionRequested
                    }
                    nori_pkl::kinds::SessionEvent::QuotaLow => SessionTrigger::QuotaLow,
                })
                .collect(),
            idle_seconds: None,
        },
        Trigger::Job { .. } => {
            return Err("a `job` trigger watches a job module's runs, not a source".to_owned());
        }
        Trigger::Output {
            command,
            cwd: command_cwd,
            stderr,
        } => Source::Command {
            command: command.clone(),
            cwd: command_cwd
                .as_deref()
                .map_or_else(|| cwd.to_path_buf(), |dir| resolve_path(cwd, dir)),
            stderr: *stderr,
        },
        Trigger::File { path, from_start } => Source::File {
            path: resolve_path(cwd, path),
            from_start: *from_start,
        },
        Trigger::Watch { paths, kinds } => Source::Watch {
            paths: paths.iter().map(|path| resolve_path(cwd, path)).collect(),
            kinds: if kinds.is_empty() {
                ChangeKind::ALL.to_vec()
            } else {
                kinds
                    .iter()
                    .map(|kind| match kind {
                        nori_pkl::kinds::WatchKind::Created => ChangeKind::Created,
                        nori_pkl::kinds::WatchKind::Modified => ChangeKind::Modified,
                        nori_pkl::kinds::WatchKind::Deleted => ChangeKind::Deleted,
                    })
                    .collect()
            },
        },
        Trigger::Http {
            url,
            interval,
            json_path,
            regex,
        } => Source::HttpPoll {
            url: url.clone(),
            interval_seconds: seconds(*interval).clamp(2, 3600),
            extract: match (json_path, regex) {
                (Some(_), Some(_)) => {
                    return Err("give `json_path` or `regex`, not both".to_owned());
                }
                (Some(path), None) => Extract::JsonPath(path.clone()),
                (None, Some(pattern)) => Extract::Regex(pattern.clone()),
                (None, None) => Extract::Status,
            },
        },
        Trigger::Ci { branch } => Source::Ci {
            sha: None,
            branch: branch.clone(),
            follow_up: None,
        },
        Trigger::Actor { actor, track } => Source::Actor {
            actor: actor.clone(),
            track: if track.is_empty() {
                Track::ALL.to_vec()
            } else {
                track
                    .iter()
                    .map(|track| match track {
                        nori_pkl::kinds::ActorTrack::State => Track::State,
                        nori_pkl::kinds::ActorTrack::Results => Track::Results,
                        nori_pkl::kinds::ActorTrack::Replies => Track::Replies,
                    })
                    .collect()
            },
        },
    })
}
