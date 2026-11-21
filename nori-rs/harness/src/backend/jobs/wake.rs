//! A job's wake is a monitor of the `job` source: the lines, the ready line and the end of the
//! job go through the same pipeline as every other monitor's, and reach the agent the same way.

use nori_machines::jobs::JobId;

use super::Followed;
use super::Jobs;
use super::spec::JobWake;
use crate::backend::monitor::Overrides;
use crate::backend::monitor::source::JOB_NOTE_PATTERN;
use crate::backend::monitor::source::SourceEnv;
use crate::backend::monitor::spec::Limits;
use crate::backend::monitor::spec::Origin;
use crate::backend::monitor::spec::Rules;
use crate::backend::monitor::spec::Source;
use crate::backend::monitor::spec::Spec;
use crate::backend::monitor::spec::WakeMode;

/// Events one job's wake may emit before it is stopped for flooding (the job runs on).
const MAX_WAKE_EVENTS: i64 = 200;

impl Jobs {
    /// Arm the monitor that wakes the agent for `id`, unless the wake asks for nothing. It replaces
    /// a running monitor of the same label. The error says why it was not armed: the job runs
    /// either way.
    pub(super) async fn arm_wake(
        &self,
        id: &JobId,
        label: &str,
        wake: &JobWake,
        followed: Followed,
    ) -> Result<(), String> {
        if !wake.is_armed() {
            return Ok(());
        }
        let source = Source::Job {
            job: id.to_string(),
            label: label.to_owned(),
            lines: wake.on_output.is_some(),
            ready: wake.ready.clone(),
        };
        let spec = Spec {
            label: label.to_owned(),
            rules: Rules {
                // The wake's own lines (ready, unreachable) must pass the regex the agent wrote.
                filter: wake
                    .on_output
                    .as_ref()
                    .map(|pattern| format!("(?:{pattern})|{JOB_NOTE_PATTERN}")),
                fail_on: Vec::new(),
                until: None,
            },
            wake: WakeMode::Immediate,
            limits: Limits {
                timeout_seconds: source.default_timeout_seconds(),
                max_events: MAX_WAKE_EVENTS,
                batch_ms: source.default_batch_ms(),
                digest_seconds: 30,
            },
            source,
            actions: Vec::new(),
            origin: Origin::Agent,
        };
        let env = SourceEnv::job(followed, wake.ready.as_deref())?;
        self.shared
            .monitors
            .start(spec, env, true)
            .await
            .map(|_| ())
            .map_err(|error| format!("the job runs, but its wake was not armed: {error}"))
    }
}

impl Jobs {
    /// Arm the wake of a job found again after a resume. The monitor the journal brought back
    /// dormant is the wake: it is started again rather than duplicated.
    pub(super) async fn rearm_wake(
        &self,
        id: &JobId,
        label: &str,
        wake: &JobWake,
        followed: Followed,
    ) -> Result<(), String> {
        let monitors = &self.shared.monitors;
        if wake.is_armed()
            && let Ok((monitor, spec, false)) = monitors.find(label)
            && matches!(&spec.source, Source::Job { job, .. } if *job == id.as_str())
        {
            let env = SourceEnv::job(followed, wake.ready.as_deref())?;
            return monitors
                .rearm(&monitor, Overrides::default(), Some(env))
                .await
                .map(|_| ())
                .map_err(|error| format!("the job runs, but its wake was not armed: {error}"));
        }
        self.arm_wake(id, label, wake, followed).await
    }
}
