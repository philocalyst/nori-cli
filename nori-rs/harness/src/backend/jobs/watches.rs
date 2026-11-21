//! Watches a `nori:Monitor` module declares on a job module (a `Job { job, on }` trigger): the
//! job may not run yet when the session starts, so the watch is kept here and armed each time a
//! job of that module starts in this session, for that run of it.
//!
//! What the trigger's `on` lists decides what the watch hears: `output` (each line, through the
//! monitor's `filter`), `ready` (the module's `ready` line, once). The run's end is reported as
//! every monitor's end is; `exited`, `failed` and `lost` are there to say it matters.

use std::sync::PoisonError;

use nori_machines::jobs::JobId;

use super::Followed;
use super::JobOrigin;
use super::Jobs;
use crate::backend::monitor::source::JOB_NOTE_PATTERN;
use crate::backend::monitor::source::SourceEnv;
use crate::backend::monitor::spec::Source;
use crate::backend::monitor::spec::Spec;

/// A monitor module's watch on the job module `job`.
#[derive(Debug, Clone)]
pub(crate) struct DeclaredWatch {
    /// The job module's name.
    pub(crate) job: String,
    /// The monitor as declared; its source is filled in for each run of the job.
    pub(crate) spec: Spec,
    /// Each output line is an event.
    pub(crate) lines: bool,
    /// The module's `ready` pattern, when the trigger asks for `ready`.
    pub(crate) ready: Option<String>,
}

impl Jobs {
    /// The watches the session's monitor modules declare, replacing the earlier set (modules are
    /// loaded again when they change). Jobs of those modules already running (a resumed session's,
    /// adopted before its modules loaded) get the watches they lack, in the background; a watch
    /// already running is left as it is.
    pub(crate) fn declare_watches(&self, watches: Vec<DeclaredWatch>) {
        *self
            .shared
            .watches
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = watches;
        let running: Vec<(JobId, String, JobOrigin)> = self
            .lock()
            .entries
            .iter()
            .filter(|entry| entry.running() && matches!(entry.origin, JobOrigin::Module { .. }))
            .map(|entry| {
                (
                    entry.record.id.clone(),
                    entry.label.clone(),
                    entry.origin.clone(),
                )
            })
            .collect();
        if running.is_empty() {
            return;
        }
        let jobs = self.clone();
        tokio::spawn(async move {
            for (id, label, origin) in running {
                for problem in jobs.arm_declared(&id, &label, &origin).await {
                    tracing::warn!(job = %id, %problem, "a declared job watch was not armed");
                }
            }
        });
    }

    /// Arm the declared watches on the job `id`, a run of the module `origin` names. Returns why
    /// any could not be armed; the job runs either way.
    pub(super) async fn arm_declared(
        &self,
        id: &JobId,
        label: &str,
        origin: &JobOrigin,
    ) -> Vec<String> {
        let JobOrigin::Module { name } = origin else {
            return Vec::new();
        };
        let watches: Vec<DeclaredWatch> = self
            .shared
            .watches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|watch| watch.job == *name)
            .cloned()
            .collect();
        let mut problems = Vec::new();
        for watch in watches {
            let DeclaredWatch {
                mut spec,
                lines,
                ready,
                ..
            } = watch;
            spec.label = format!("{}:{label}", spec.label);
            if matches!(self.shared.monitors.find(&spec.label), Ok((_, _, true))) {
                continue;
            }
            spec.source = Source::Job {
                job: id.to_string(),
                label: label.to_owned(),
                lines,
                ready: ready.clone(),
            };
            // The watch's own lines (ready, unreachable) pass whatever filter the module wrote.
            spec.rules.filter = spec
                .rules
                .filter
                .map(|pattern| format!("(?:{pattern})|{JOB_NOTE_PATTERN}"));
            let armed = match self.follow(&id.to_string()) {
                Ok(followed @ Followed::Live { .. }) => {
                    match SourceEnv::job(followed, ready.as_deref()) {
                        Ok(env) => self
                            .shared
                            .monitors
                            .start(spec.clone(), env, true)
                            .await
                            .map(|_| ()),
                        Err(error) => Err(error),
                    }
                }
                // Over before it could be watched: nothing to arm.
                Ok(Followed::Over(_)) => Ok(()),
                Err(error) => Err(error),
            };
            if let Err(error) = armed {
                problems.push(format!("monitor `{}` on job `{name}`: {error}", spec.label));
            }
        }
        problems
    }
}
