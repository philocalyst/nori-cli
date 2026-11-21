//! What feeds a monitor: one task per source, sending [`Item`]s to the monitor's pipeline.
//!
//! A source never decides what is an event; it says what it saw, line by line, flags what it knows
//! is a failure, and ends with how it ended. The pipeline applies the monitor's rules to it.

mod actor;
mod ci;
mod command;
mod file;
mod http;
mod job;
mod jsonpath;
mod schedule;
mod session;
mod watch;

use std::path::Path;
use std::path::PathBuf;

use tokio::sync::mpsc::Sender;

use super::event::Ending;
use super::spec::Source;
use crate::backend::orchestrator::ActorWatch;
use crate::backend::orchestrator::Orchestrator;

pub(super) use ci::CiTarget;
pub(crate) use job::NOTE_PATTERN as JOB_NOTE_PATTERN;
pub(crate) use schedule::Schedule;

/// What a source tells its monitor.
pub(super) enum Item {
    Line {
        text: String,
        failure: bool,
    },
    /// How things stand when the source starts: an event only if a `fail_on` or `until` rule
    /// matches it.
    Baseline {
        text: String,
    },
    /// The source is over, and this is how.
    Done(Ending),
}

/// What a source needs that was resolved when the monitor was armed, so a mistake in it is the
/// arming call's error and not a monitor that dies a moment later.
#[derive(Default)]
pub(crate) struct SourceEnv {
    actor: Option<ActorWatch>,
    ci: Option<CiTarget>,
    schedule: Option<schedule::Compiled>,
    /// The session's events, for a `session` source.
    events: Option<tokio::sync::mpsc::Receiver<nori_protocol::SessionEvent>>,
    /// The job a `job` source follows.
    job: Option<job::Prepared>,
}

impl SourceEnv {
    /// A `job` source following `followed`; `ready` is checked now.
    pub(crate) fn job(
        followed: crate::backend::jobs::Followed,
        ready: Option<&str>,
    ) -> Result<Self, String> {
        Ok(Self {
            job: Some(job::Prepared::new(followed, ready)?),
            ..Self::default()
        })
    }

    /// Check `source` against the actor's sandbox and the world, and resolve what it names.
    pub(crate) async fn prepare(
        source: &Source,
        orchestrator: &Orchestrator,
        sandbox: Option<&nori_conduit::SandboxPolicy>,
        cwd: &Path,
    ) -> Result<Self, String> {
        let allowed = |path: &Path| {
            nori_conduit::enforce_sandbox_path(sandbox, cwd, &path.to_string_lossy())
                .map_err(|error| error.to_string())
        };
        let mut env = Self::default();
        match source {
            Source::Command {
                command,
                cwd: command_cwd,
                ..
            } => {
                nori_conduit::enforce_bash_sandbox(sandbox, command_cwd, command)
                    .map_err(|error| error.to_string())?;
                if !command_cwd.is_dir() {
                    return Err(format!(
                        "the working directory {} does not exist",
                        command_cwd.display()
                    ));
                }
            }
            Source::File { path, .. } => {
                allowed(path)?;
                if !path.is_file() {
                    return Err(format!(
                        "{} is not a file that exists. Create it first, or use a `watch` on its \
                         directory to see it appear.",
                        path.display()
                    ));
                }
            }
            Source::Watch { paths, .. } => {
                for path in paths {
                    allowed(path)?;
                    if !path.exists() {
                        return Err(format!(
                            "{} does not exist. Watch its parent directory to see it appear.",
                            path.display()
                        ));
                    }
                }
            }
            Source::Actor { actor, .. } => env.actor = Some(orchestrator.watch_actor(actor)?),
            Source::Ci { sha, branch, .. } => {
                env.ci = Some(CiTarget::resolve(cwd, sha.as_deref(), branch.as_deref()).await?);
            }
            Source::HttpPoll { url, .. } => http::check(url)?,
            Source::Timer { schedule } => {
                env.schedule = Some(schedule.compile(chrono::Local::now())?);
            }
            Source::Session {
                events,
                idle_seconds,
            } => {
                if events.is_empty() && idle_seconds.is_none() {
                    return Err(
                        "a `session` monitor needs `events`, `idle_seconds`, or both".to_owned(),
                    );
                }
                if idle_seconds.is_some_and(|seconds| seconds < 1) {
                    return Err("`idle_seconds` must be at least 1".to_owned());
                }
                env.events = Some(orchestrator.monitors().subscribe_events().ok_or(
                    "this session does not publish its events to monitors (a headless child)",
                )?);
            }
            Source::Job { job, ready, .. } => {
                return Self::job(orchestrator.jobs().follow(job)?, ready.as_deref());
            }
        }
        Ok(env)
    }
}

/// Run `source` until it ends, reporting to `tx`. The end is always the last item sent; a source
/// that cannot say how it ended is reported by the pipeline as failed.
pub(super) async fn run(source: Source, env: SourceEnv, tx: Sender<Item>) {
    let SourceEnv {
        actor,
        ci,
        schedule,
        events,
        job,
    } = env;
    let ending = match source {
        Source::Command {
            command,
            cwd,
            stderr,
        } => command::run(&command, cwd, stderr, &tx).await,
        Source::File { path, from_start } => file::run(path, from_start, &tx).await,
        Source::Watch { paths, kinds } => watch::run(paths, &kinds, &tx).await,
        Source::Actor { track, .. } => match actor {
            Some(actor) => actor::run(actor, &track, &tx).await,
            None => Some(missing("actor")),
        },
        Source::Ci { follow_up, .. } => match ci {
            Some(target) => ci::run(target, follow_up, &tx).await,
            None => Some(missing("ci")),
        },
        Source::HttpPoll {
            url,
            interval_seconds,
            extract,
        } => http::run(&url, interval_seconds, &extract, &tx).await,
        Source::Timer { .. } => match schedule {
            Some(schedule) => schedule::run(schedule, &tx).await,
            None => Some(missing("timer")),
        },
        Source::Session {
            events: triggers,
            idle_seconds,
        } => match events {
            Some(events) => {
                let idle = idle_seconds
                    .map(|seconds| std::time::Duration::from_secs(seconds.max(1).unsigned_abs()));
                session::run(events, &triggers, idle, &tx).await
            }
            None => Some(missing("session")),
        },
        Source::Job { lines, .. } => match job {
            Some(job) => job::run(job, lines, &tx).await,
            None => Some(missing("job")),
        },
    };
    if let Some(ending) = ending {
        let _ = tx.send(Item::Done(ending)).await;
    }
}

fn missing(source: &str) -> Ending {
    Ending::Failed {
        reason: format!("the {source} source was started without being prepared"),
    }
}

/// `requested` resolved against `cwd`.
pub(crate) fn resolve_path(cwd: &Path, requested: &str) -> PathBuf {
    let path = PathBuf::from(requested);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}
