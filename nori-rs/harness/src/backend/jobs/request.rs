//! The arguments of the job tools, and how `job_start` becomes a [`StartJob`].

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use nori_machines::jobs::Signal;
use nori_pkl::catalog::Catalog;
use rmcp::schemars;
use serde::Deserialize;

use super::JobOrigin;
use super::JobWake;
use super::ReadSpec;
use super::StartJob;

/// The longest a stop waits between TERM and KILL.
const MAX_GRACE_SECONDS: i64 = 300;
/// How long a stop waits between TERM and KILL when not told.
const DEFAULT_GRACE_SECONDS: i64 = 5;
/// How much of each stream a read returns when not told.
const DEFAULT_READ_BYTES: i64 = 16 * 1024;

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct JobStartRequest {
    /// The shell command to run. Required unless `job` names a module.
    #[serde(default)]
    command: Option<String>,
    /// Instead of `command`: the name of a job module (`jobs/NAME.pkl`). Its command, machine,
    /// directory, environment, `ready` and wake are used; the fields here override them.
    #[serde(default)]
    job: Option<String>,
    /// A short name, such as `dev` or `tests`: it addresses the job in `job_read` and `job_stop`
    /// and heads its wakes. Unique among your running jobs. Default: from the command.
    #[serde(default)]
    label: Option<String>,
    /// Where to run: `local` (this computer, the default) or one of the user's machines, as for
    /// `bash`.
    #[serde(default)]
    machine: Option<String>,
    /// The directory to start in; it must exist (`~/x` is under a machine's home). Default: your
    /// shell's current directory.
    #[serde(default)]
    cwd: Option<String>,
    /// Environment variables for this job only; never written to disk on any machine.
    #[serde(default)]
    env: Option<BTreeMap<String, String>>,
    /// Wake you when the job exits (default true). A non-zero exit, a kill or a lost job always
    /// wakes you while any wake is armed.
    #[serde(default)]
    wake_on_exit: Option<bool>,
    /// A regular expression: each output line that matches wakes you (lines close together are
    /// one wake). Use it for errors, not for everything.
    #[serde(default)]
    wake_on_output: Option<String>,
    /// A regular expression: the first output line that matches wakes you once, as "ready" (such
    /// as `Listening on|Local:`).
    #[serde(default)]
    ready: Option<String>,
}

impl JobStartRequest {
    /// Start the job module `name` as it is declared, as a monitor's `run_job` action does.
    pub(crate) fn module(name: &str) -> Self {
        Self {
            command: None,
            job: Some(name.to_owned()),
            label: None,
            machine: None,
            cwd: None,
            env: None,
            wake_on_exit: None,
            wake_on_output: None,
            ready: None,
        }
    }

    /// The job this asks for. A module supplies what the request leaves out; paths are relative to
    /// `cwd`.
    pub(crate) fn into_start(self, modules: &Catalog, cwd: &Path) -> Result<StartJob, String> {
        let (origin, command, machine, dir, mut env, mut wake, default_label) =
            match (self.job.as_deref(), self.command) {
                (Some(_), Some(_)) => {
                    return Err("give `command` or `job` (a module's name), not both".to_owned());
                }
                (None, None) => {
                    return Err(
                        "`job_start` needs a `command`, or `job` naming a module".to_owned()
                    );
                }
                (None, Some(command)) => (
                    JobOrigin::Agent,
                    command,
                    None,
                    None,
                    BTreeMap::new(),
                    JobWake::default(),
                    None,
                ),
                (Some(name), None) => {
                    let Some(module) = modules.job(name) else {
                        let known: Vec<&str> =
                            modules.jobs.iter().map(|job| job.name.as_str()).collect();
                        return Err(if known.is_empty() {
                            format!("there is no job module named `{name}`: none are loaded")
                        } else {
                            format!(
                                "there is no job module named `{name}`. The modules are: {}",
                                known.join(", ")
                            )
                        });
                    };
                    let def = &module.value;
                    // TODO(jobs): `restart` (policy, max, backoff), `check`, `timeout`,
                    // `ready.timeout` and `max_log_bytes` are not applied yet; the module still
                    // starts, once.
                    (
                        JobOrigin::Module {
                            name: name.to_owned(),
                        },
                        def.command.clone(),
                        Some(def.machine.clone()),
                        def.cwd.clone(),
                        def.env.clone(),
                        JobWake {
                            on_exit: def.wake.on_exit,
                            on_output: def.wake.on_output.clone(),
                            ready: def
                                .ready
                                .as_ref()
                                .filter(|_| def.wake.on_ready)
                                .map(|ready| ready.pattern.clone()),
                        },
                        Some(name.to_owned()),
                    )
                }
            };
        env.extend(self.env.unwrap_or_default());
        if let Some(on_exit) = self.wake_on_exit {
            wake.on_exit = on_exit;
        }
        wake.on_output = self.wake_on_output.or(wake.on_output);
        wake.ready = self.ready.or(wake.ready);
        for (field, pattern) in [("wake_on_output", &wake.on_output), ("ready", &wake.ready)] {
            if let Some(pattern) = pattern
                && let Err(error) = regex::Regex::new(pattern)
            {
                return Err(format!(
                    "`{field}` is not a valid regular expression ({pattern:?}): {error}"
                ));
            }
        }
        Ok(StartJob {
            command,
            label: self.label.or(default_label),
            machine: self.machine.or(machine),
            cwd: self.cwd.or(dir),
            env,
            origin,
            wake,
            local_cwd: cwd.to_path_buf(),
        })
    }
}

/// Where `job_read` starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReadFromKind {
    /// What you have not read yet.
    New,
    /// The last `max_bytes` of each stream.
    Tail,
    /// The very beginning.
    Start,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct JobReadRequest {
    /// Required. The job's id or label.
    #[serde(alias = "id", alias = "label", alias = "target")]
    pub(crate) job: String,
    /// `new` (default), `tail` or `start`.
    #[serde(default)]
    from: Option<ReadFromKind>,
    /// At most this many bytes of each stream (default 16384, at most 262144).
    #[serde(default)]
    max_bytes: Option<i64>,
}

impl JobReadRequest {
    pub(crate) fn read(&self) -> (ReadSpec, i64) {
        let max_bytes = self.max_bytes.unwrap_or(DEFAULT_READ_BYTES);
        let from = match self.from.unwrap_or(ReadFromKind::New) {
            ReadFromKind::New => ReadSpec::New,
            ReadFromKind::Tail => ReadSpec::Tail { bytes: max_bytes },
            ReadFromKind::Start => ReadSpec::Start,
        };
        (from, max_bytes)
    }
}

/// The signals `job_stop` may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum SignalName {
    Int,
    Term,
    Hup,
    Kill,
    Usr1,
    Usr2,
}

#[derive(Deserialize, schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
#[serde(deny_unknown_fields)]
pub(crate) struct JobStopRequest {
    /// Required. The job's id or label.
    #[serde(alias = "id", alias = "label", alias = "target")]
    pub(crate) job: String,
    /// Send only this signal (`INT`, `TERM`, `HUP`, `KILL`, `USR1`, `USR2`) and leave the rest to
    /// the job. Default: TERM, then KILL after `grace_seconds`.
    #[serde(default)]
    signal: Option<SignalName>,
    /// How long to wait after TERM before KILL (default 5, at most 300).
    #[serde(default)]
    grace_seconds: Option<i64>,
}

impl JobStopRequest {
    pub(crate) fn stop(&self) -> (Option<Signal>, Duration) {
        let signal = self.signal.map(|signal| match signal {
            SignalName::Int => Signal::Int,
            SignalName::Term => Signal::Term,
            SignalName::Hup => Signal::Hup,
            SignalName::Kill => Signal::Kill,
            SignalName::Usr1 => Signal::Usr1,
            SignalName::Usr2 => Signal::Usr2,
        });
        let grace = self
            .grace_seconds
            .unwrap_or(DEFAULT_GRACE_SECONDS)
            .clamp(0, MAX_GRACE_SECONDS);
        (
            signal,
            Duration::from_secs(u64::try_from(grace).unwrap_or(0)),
        )
    }
}
