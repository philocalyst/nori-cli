//! A session as a plugin host: each operation of the plugin API is something the session already
//! does for its tools and its person, called here and never rebuilt.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nori_pkl::catalog::Catalog;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

use super::executor;
use super::host::Exported;
use super::host::JobOutcome;
use super::host::PluginHost;
use super::host::SkillOutcome;
use super::types::JobDescription;
use super::types::MonitorDescription;
use super::types::PermissionDescription;
use super::types::PluginError;
use super::types::PluginInfo;
use super::types::PluginRun;
use super::types::SessionDescription;
use super::types::SessionSetting;
use crate::backend::AcpBackend;
use crate::backend::BackendEvent;
use crate::backend::PromptAdmission;
use crate::backend::jobs::JobStartRequest;
use crate::backend::monitor::Ending;
use crate::backend::monitor::standing;

/// How long a plugin's `job.stop` lets a job end before it is killed.
const JOB_GRACE: Duration = Duration::from_secs(5);

impl AcpBackend {
    /// The plugins this session offers.
    pub(crate) async fn plugins(&self) -> Vec<PluginInfo> {
        let modules = self.orchestrator.modules().await;
        PluginInfo::of(&modules)
    }

    /// Run a plugin's command on this session and say how it went in the transcript. It waits on
    /// what its operations wait on (a skill, a job's stop), so callers run it off the event loop.
    pub(crate) async fn run_plugin(
        &self,
        plugin: &str,
        command: &str,
        arguments: BTreeMap<String, String>,
    ) -> Result<PluginRun, PluginError> {
        let outcome = executor::run(self, plugin, command, arguments).await;
        match &outcome {
            Ok(run) => {
                let steps = run.steps.len();
                let plural = if steps == 1 { "" } else { "s" };
                self.notice(&format!(
                    "plugin {plugin} {command}: {steps} step{plural} done"
                ))
                .await;
            }
            Err(PluginError::StepFailed {
                at,
                message,
                completed,
            }) => {
                let done = completed.len();
                self.notice(&format!(
                    "plugin {plugin} {command} stopped at {at} after {done} done: {message}"
                ))
                .await;
            }
            // The caller's mistake, found before anything ran: they hear it as the error.
            Err(
                PluginError::UnknownPlugin { .. }
                | PluginError::UnknownCommand { .. }
                | PluginError::UnknownArgument { .. }
                | PluginError::MissingArgument { .. }
                | PluginError::UnknownPlaceholder { .. }
                | PluginError::UnknownAction { .. }
                | PluginError::BadArgument { .. }
                | PluginError::SessionClosed,
            ) => {}
        }
        outcome
    }
}

impl PluginHost for AcpBackend {
    async fn modules(&self) -> Arc<Catalog> {
        self.orchestrator.modules().await
    }

    async fn describe(&self) -> SessionDescription {
        let machine = tokio::task::spawn_blocking(crate::portable::hostname)
            .await
            .unwrap_or_else(|_| "unknown".to_owned());
        let settings = self
            .config_options()
            .iter()
            .filter_map(|option| {
                let acp::SessionConfigKind::Select(select) = &option.kind else {
                    return None;
                };
                Some(SessionSetting {
                    id: option.id.0.to_string(),
                    name: option.name.clone(),
                    value: select.current_value.0.to_string(),
                })
            })
            .collect();
        let jobs = self
            .orchestrator
            .jobs()
            .list()
            .into_iter()
            .map(|job| JobDescription {
                label: job.label,
                command: job.command,
                machine: job.place,
                state: job.state,
            })
            .collect();
        let monitors = self
            .orchestrator
            .monitors()
            .list()
            .into_iter()
            .map(|monitor| MonitorDescription {
                label: monitor.label,
                watching: monitor.watching,
                state: monitor.state,
            })
            .collect();
        SessionDescription {
            agent: self.agent_name().to_owned(),
            conversation_id: self.conversation_id.read().await.to_string(),
            cwd: self.cwd.clone(),
            machine,
            settings,
            jobs,
            monitors,
            permissions: PermissionDescription {
                approval_policy: *self.approval_policy_tx.borrow(),
                sandbox: self.orchestrator.config().sandbox_policy.clone(),
            },
        }
    }

    async fn export(&self, path: Option<PathBuf>) -> Result<Exported, String> {
        let conversation_id = self.conversation_id.read().await.to_string();
        self.flush_transcript()
            .await
            .map_err(|error| format!("could not flush the transcript: {error:#}"))?;
        let bundle = crate::portable::export(&self.nori_home, &conversation_id)
            .await
            .map_err(|error| error.to_string())?;
        let path = match path {
            Some(path) if path.is_absolute() => path,
            Some(path) => self.cwd.join(path),
            None => self
                .nori_home
                .join("exports")
                .join(format!("{conversation_id}.nori-session")),
        };
        let bytes = tokio::task::spawn_blocking(move || bundle.encode())
            .await
            .map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
        }
        tokio::fs::write(&path, &bytes)
            .await
            .map_err(|error| format!("could not write {}: {error}", path.display()))?;
        Ok(Exported {
            path,
            conversation_id,
            bytes: i64::try_from(bytes.len()).unwrap_or(i64::MAX),
        })
    }

    async fn request_move(&self, machine: &str) -> Result<(), String> {
        match nori_machines::global_ready().await.resolve(Some(machine)) {
            Err(error) => return Err(error.to_string()),
            Ok(nori_machines::Target::Local) => {
                return Err(format!(
                    "{machine} is this computer; the session is already here"
                ));
            }
            Ok(nori_machines::Target::Remote(_)) => {}
        }
        self.backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::SessionMoveRequested {
                    machine: machine.to_owned(),
                },
            )))
            .await
            .map_err(|_| "the session is ending".to_owned())
    }

    async fn start_job(&self, job: &str) -> Result<JobOutcome, String> {
        let modules = self.orchestrator.modules().await;
        let start = JobStartRequest::module(job).into_start(&modules, &self.cwd)?;
        let started = self.orchestrator.jobs().start(start).await?;
        Ok(JobOutcome {
            job: started.job,
            label: started.label,
            state: started.state,
        })
    }

    async fn stop_job(&self, job: &str) -> Result<JobOutcome, String> {
        let stopped = self.orchestrator.jobs().stop(job, None, JOB_GRACE).await?;
        Ok(JobOutcome {
            job: stopped.job,
            label: stopped.label,
            state: stopped.state,
        })
    }

    async fn start_monitor(&self, monitor: &str) -> Result<Vec<String>, String> {
        let modules = self.orchestrator.modules().await;
        let Some(module) = modules.monitor(monitor) else {
            let known: Vec<&str> = modules
                .monitors
                .iter()
                .map(|module| module.name.as_str())
                .collect();
            return Err(if known.is_empty() {
                format!("there is no monitor module named `{monitor}`: none are loaded")
            } else {
                format!(
                    "there is no monitor module named `{monitor}`. The modules are: {}",
                    known.join(", ")
                )
            });
        };
        // The watches on job modules were declared when the modules loaded.
        let arming = self.arm_monitor_module(module, &modules).await;
        if arming.problems.is_empty() {
            Ok(arming.armed)
        } else {
            Err(arming.problems.join("; "))
        }
    }

    async fn stop_monitor(&self, monitor: &str) -> Result<Vec<String>, String> {
        let modules = self.orchestrator.modules().await;
        // A module with several triggers arms one monitor per trigger, each with its own label.
        let mut labels: Vec<String> = modules
            .monitor(monitor)
            .and_then(|module| {
                standing::specs(&module.value, &module.path, &self.cwd, &modules).ok()
            })
            .map(|standing| standing.armed.into_iter().map(|spec| spec.label).collect())
            .unwrap_or_default();
        if labels.is_empty() {
            labels.push(monitor.to_owned());
        }
        let mut stopped = Vec::new();
        let mut refused = Vec::new();
        for label in labels {
            match self
                .orchestrator
                .monitors()
                .stop(&label, Ending::Stopped)
                .await
            {
                Ok(done) => stopped.extend(done.into_iter().map(|done| done.label)),
                Err(error) => refused.push(error),
            }
        }
        if stopped.is_empty() {
            Err(refused.join("; "))
        } else {
            Ok(stopped)
        }
    }

    async fn run_skill(
        &self,
        skill: &str,
        arguments: BTreeMap<String, String>,
    ) -> Result<SkillOutcome, String> {
        let result = self
            .orchestrator
            .execute_skill(skill, arguments)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok(SkillOutcome {
            journal: result.journal,
            value: result.value,
        })
    }

    async fn notify(&self, title: Option<String>, body: Option<String>) -> String {
        self.tell_person("plugin", title, body.unwrap_or_default(), false)
            .await
    }

    async fn queue_prompt(&self, text: String) -> Result<(), String> {
        let backend = self.clone();
        // Admission waits for a running turn; the next operation must not wait with it.
        tokio::spawn(async move {
            let (response_tx, response_rx) = tokio::sync::oneshot::channel();
            // Policy admission reads a dropped cancel sender as a cancel: this one lives until
            // the prompt has been answered for.
            let (_keep_open, cancellation) = tokio::sync::watch::channel(false);
            backend
                .submit_prompt_cancellable(
                    vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                    None,
                    PromptAdmission::Queue,
                    cancellation,
                    response_tx,
                )
                .await;
            if let Ok(Err(error)) = response_rx.await {
                backend
                    .notice(&format!("plugin: a prompt was not queued: {error:#}"))
                    .await;
            }
        });
        Ok(())
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        let options = self.config_options();
        if !options.iter().any(|option| option.id.0.as_ref() == key) {
            let known: Vec<&str> = options.iter().map(|option| option.id.0.as_ref()).collect();
            return Err(if known.is_empty() {
                format!("the agent offers no setting `{key}`: it offers none")
            } else {
                format!(
                    "the agent offers no setting `{key}`. Its settings are: {}",
                    known.join(", ")
                )
            });
        }
        self.set_config_option(key.to_owned(), value.to_owned())
            .await
            .map_err(|error| format!("{error:#}"))
    }
}
