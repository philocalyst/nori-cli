//! The Pkl runtime's modules for a session ([docs/pkl-runtime.md]): jobs, monitors, skills,
//! plugins and gates from `$NORI_HOME/<kind>/` and, in a trusted project, `<repo>/.nori/<kind>/`.
//!
//! They are evaluated on a thread of their own (a module may read any file the person names, and
//! nothing on the event loop waits for that), published on the orchestrator, and acted on: each
//! module that does not load is told once, with its `file:line`, and each monitor module is armed
//! as a standing monitor. A child session has none: its parent's modules are the session's.

use std::sync::Arc;

use nori_pkl::catalog::Catalog;
use nori_pkl::catalog::Loaded;
use nori_pkl::catalog::Roots;
use nori_pkl::kinds::MonitorDef;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;

use super::AcpBackend;
use super::BackendEvent;
use super::monitor::source::SourceEnv;
use super::monitor::standing;

/// What arming a monitor module did.
#[derive(Debug, Default)]
pub(in crate::backend) struct Arming {
    /// The labels of the monitors armed.
    pub(in crate::backend) armed: Vec<String>,
    /// Watches on job modules, armed when those jobs run.
    pub(in crate::backend) on_jobs: Vec<super::jobs::DeclaredWatch>,
    /// Why anything could not be armed, with the module's file.
    pub(in crate::backend) problems: Vec<String>,
}

impl AcpBackend {
    /// Load the session's modules in the background and arm what they declare.
    pub(in crate::backend) fn load_pkl_modules(&self) {
        if self.orchestrator.is_child() {
            self.orchestrator.publish_modules(Arc::default());
            return;
        }
        let roots = Roots {
            user: Some(self.nori_home.clone()),
            project: self
                .orchestrator
                .project_trusted()
                .then(|| self.cwd.join(".nori")),
        };
        // Until the first load ends, a request waits for the gates rather than pass them unseen.
        self.orchestrator.seed().tree.gates.expect_load();
        let backend = self.clone();
        tokio::spawn(async move {
            let (loaded_tx, loaded) = tokio::sync::oneshot::channel();
            // Not the blocking pool: tokio waits for it at shutdown, and a module's reads may not
            // return. A detached thread, as workflow policy loads.
            let spawned = std::thread::Builder::new()
                .name("nori-pkl-modules".into())
                .spawn(move || {
                    // Skills are programs: their schema (`nori:Skill`, `nori:Program`) is conduit's.
                    let shipped = roots
                        .user
                        .as_deref()
                        .map(super::builtin_modules::materialize);
                    let mut catalog = nori_pkl::catalog::load(
                        &roots,
                        &nori_conduit::schemas::program_evaluator_options(),
                    );
                    if let Some(Err(problem)) = shipped {
                        catalog.problems.push(problem);
                    }
                    let _ = loaded_tx.send(catalog);
                });
            if let Err(error) = spawned {
                tracing::warn!(%error, "could not start the Pkl module loader");
                backend
                    .orchestrator
                    .seed()
                    .tree
                    .gates
                    .publish(Arc::default());
                return;
            }
            let Ok(catalog) = loaded.await else {
                backend
                    .orchestrator
                    .seed()
                    .tree
                    .gates
                    .publish(Arc::default());
                return;
            };
            backend.apply_pkl_modules(Arc::new(catalog)).await;
        });
    }

    /// The session's skills, as their modules declare them.
    pub(crate) async fn skills(&self) -> Vec<crate::runtime::SkillInfo> {
        self.orchestrator
            .modules()
            .await
            .skills
            .iter()
            .map(|skill| crate::runtime::SkillInfo {
                name: skill.name.clone(),
                summary: skill.value.summary.clone(),
                command: skill
                    .value
                    .command
                    .clone()
                    .unwrap_or_else(|| skill.name.clone()),
                parameters: skill
                    .value
                    .parameters
                    .iter()
                    .map(|(name, parameter)| crate::runtime::SkillParameterInfo {
                        name: name.clone(),
                        description: parameter.description.clone(),
                        required: parameter.required && parameter.default.is_none(),
                    })
                    .collect(),
                path: skill.path.clone(),
            })
            .collect()
    }

    /// Arm what the monitor module `module` declares, a monitor per trigger. Standing monitors at
    /// load and a plugin's `monitor.start` arm a module the same way.
    pub(in crate::backend) async fn arm_monitor_module(
        &self,
        module: &Loaded<MonitorDef>,
        catalog: &Catalog,
    ) -> Arming {
        let mut arming = Arming::default();
        let standing = match standing::specs(&module.value, &module.path, &self.cwd, catalog) {
            Ok(standing) => standing,
            Err(error) => {
                arming
                    .problems
                    .push(format!("{}: {error}", module.path.display()));
                return arming;
            }
        };
        arming.on_jobs = standing.on_jobs;
        for spec in standing.armed {
            let label = spec.label.clone();
            let armed = async {
                let env =
                    SourceEnv::prepare(&spec.source, &self.orchestrator, None, &self.cwd).await?;
                self.orchestrator.monitors().start(spec, env, true).await
            }
            .await;
            match armed {
                Ok(_) => arming.armed.push(label),
                Err(error) => arming.problems.push(format!(
                    "{}: monitor `{label}` could not start: {error}",
                    module.path.display()
                )),
            }
        }
        arming
    }

    async fn apply_pkl_modules(&self, catalog: Arc<Catalog>) {
        let mut problems: Vec<String> = catalog
            .problems
            .iter()
            .map(|problem| problem.message.clone())
            .collect();
        // Gates first: they decide what the agent asks for, and arming monitors can take a while.
        let (gates, gate_problems) = super::gates::Gates::compile(&catalog);
        self.orchestrator.seed().tree.gates.publish(Arc::new(gates));
        problems.extend(gate_problems);
        let mut on_jobs = Vec::new();
        for module in &catalog.monitors {
            let arming = self.arm_monitor_module(module, &catalog).await;
            on_jobs.extend(arming.on_jobs);
            problems.extend(arming.problems);
        }
        self.orchestrator.jobs().declare_watches(on_jobs);
        self.orchestrator.publish_modules(catalog);
        for problem in problems {
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                    nori_protocol::Notice {
                        message: format!("pkl: {problem}"),
                    },
                ))))
                .await;
        }
    }
}
