//! The executor against a host that records what it was asked: arguments, the order and mapping
//! of each operation, and what a failure leaves behind. The session's own half of each operation
//! is proven end to end in `world/tests/plugins.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use nori_config::AskForApproval;
use nori_config::SandboxPolicy;
use nori_pkl::catalog::Catalog;
use nori_pkl::catalog::Loaded;
use nori_pkl::catalog::Scope;
use nori_pkl::kinds::Command;
use nori_pkl::kinds::CommandCategory;
use nori_pkl::kinds::CommandEffect;
use nori_pkl::kinds::CommandRequires;
use nori_pkl::kinds::Operation;
use nori_pkl::kinds::Parameter;
use nori_pkl::kinds::PluginDef;
use pretty_assertions::assert_eq;

use super::executor::run;
use super::host::Exported;
use super::host::JobOutcome;
use super::host::PluginHost;
use super::host::SkillOutcome;
use super::types::CommandInfo;
use super::types::ParameterInfo;
use super::types::PluginError;
use super::types::PluginInfo;
use super::types::SessionDescription;
use super::types::StepResult;

/// A host that answers every operation and writes down the call.
struct Recorder {
    catalog: Arc<Catalog>,
    calls: Mutex<Vec<String>>,
    /// The call (its name, such as `session.move`) that fails with "refused".
    failing: Option<&'static str>,
}

impl Recorder {
    fn new(plugin: PluginDef) -> Self {
        Self {
            catalog: Arc::new(Catalog {
                plugins: vec![Loaded {
                    name: plugin.name.clone(),
                    path: PathBuf::from("/home/plugins/test.pkl"),
                    scope: Scope::User,
                    value: plugin,
                }],
                ..Catalog::default()
            }),
            calls: Mutex::default(),
            failing: None,
        }
    }

    fn failing(mut self, call: &'static str) -> Self {
        self.failing = Some(call);
        self
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn record(&self, call: &'static str, detail: String) -> Result<(), String> {
        self.calls.lock().unwrap().push(format!("{call}: {detail}"));
        if self.failing == Some(call) {
            Err("refused".to_owned())
        } else {
            Ok(())
        }
    }
}

impl PluginHost for Recorder {
    async fn modules(&self) -> Arc<Catalog> {
        Arc::clone(&self.catalog)
    }

    async fn describe(&self) -> SessionDescription {
        let _ = self.record("session.describe", String::new());
        SessionDescription {
            agent: "fake".to_owned(),
            conversation_id: "conv-1".to_owned(),
            cwd: PathBuf::from("/work"),
            machine: "this-mac".to_owned(),
            settings: Vec::new(),
            jobs: Vec::new(),
            monitors: Vec::new(),
            permissions: super::types::PermissionDescription {
                approval_policy: AskForApproval::OnRequest,
                sandbox: SandboxPolicy::DangerFullAccess,
            },
        }
    }

    async fn export(&self, path: Option<PathBuf>) -> Result<Exported, String> {
        self.record("session.export", format!("{path:?}"))?;
        Ok(Exported {
            path: path.unwrap_or_else(|| PathBuf::from("/home/exports/conv-1.nori-session")),
            conversation_id: "conv-1".to_owned(),
            bytes: 12,
        })
    }

    async fn request_move(&self, machine: &str) -> Result<(), String> {
        self.record("session.move", machine.to_owned())
    }

    async fn start_job(&self, job: &str) -> Result<JobOutcome, String> {
        self.record("job.start", job.to_owned())?;
        Ok(JobOutcome {
            job: "job-1".to_owned(),
            label: job.to_owned(),
            state: "running".to_owned(),
        })
    }

    async fn stop_job(&self, job: &str) -> Result<JobOutcome, String> {
        self.record("job.stop", job.to_owned())?;
        Ok(JobOutcome {
            job: "job-1".to_owned(),
            label: job.to_owned(),
            state: "killed".to_owned(),
        })
    }

    async fn start_monitor(&self, monitor: &str) -> Result<Vec<String>, String> {
        self.record("monitor.start", monitor.to_owned())?;
        Ok(vec![format!("{monitor}:1"), format!("{monitor}:2")])
    }

    async fn stop_monitor(&self, monitor: &str) -> Result<Vec<String>, String> {
        self.record("monitor.stop", monitor.to_owned())?;
        Ok(vec![monitor.to_owned()])
    }

    async fn run_skill(
        &self,
        skill: &str,
        arguments: BTreeMap<String, String>,
    ) -> Result<SkillOutcome, String> {
        self.record("skill.run", format!("{skill} {arguments:?}"))?;
        Ok(SkillOutcome {
            journal: "/runs/1/journal".to_owned(),
            value: serde_json::json!("decided"),
        })
    }

    async fn notify(&self, title: Option<String>, body: Option<String>) -> String {
        let _ = self.record("notify", format!("{title:?} {body:?}"));
        title.unwrap_or_else(|| "plugin".to_owned())
    }

    async fn queue_prompt(&self, text: String) -> Result<(), String> {
        self.record("prompt", text)
    }

    async fn set_config(&self, key: &str, value: &str) -> Result<(), String> {
        self.record("config.set", format!("{key}={value}"))
    }
}

fn parameter(default: Option<&str>) -> Parameter {
    Parameter {
        description: "a parameter".to_owned(),
        default: default.map(str::to_owned),
        required: default.is_none(),
        choices: Vec::new(),
        from: None,
    }
}

fn plugin(parameters: &[(&str, Parameter)], operations: Vec<Operation>) -> PluginDef {
    PluginDef {
        name: "test".to_owned(),
        description: "a plugin to test with".to_owned(),
        commands: BTreeMap::from([(
            "go".to_owned(),
            Command {
                description: "go".to_owned(),
                title: None,
                names: Vec::new(),
                keywords: Vec::new(),
                category: CommandCategory::Plugin,
                effect: CommandEffect::Now,
                requires: CommandRequires::default(),
                parameters: parameters
                    .iter()
                    .map(|(name, parameter)| ((*name).to_owned(), parameter.clone()))
                    .collect(),
                operations,
            },
        )]),
    }
}

fn arguments(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

/// A command that uses an operation of every kind, each with a parameter in a text field.
fn every_operation() -> PluginDef {
    plugin(
        &[
            ("machine", parameter(None)),
            ("job", parameter(Some("web"))),
            ("effort", parameter(Some("high"))),
        ],
        vec![
            Operation::SessionDescribe {},
            Operation::SessionExport {
                path: Some("out/{{machine}}.bundle".to_owned()),
            },
            Operation::SessionMove {
                machine: "{{machine}}".to_owned(),
            },
            Operation::JobStart {
                job: "{{job}}".to_owned(),
            },
            Operation::JobStop {
                job: "{{ job }}".to_owned(),
            },
            Operation::MonitorStart {
                monitor: "watch-{{job}}".to_owned(),
            },
            Operation::MonitorStop {
                monitor: "watch-{{job}}".to_owned(),
            },
            Operation::SkillRun {
                skill: "council".to_owned(),
                arguments: BTreeMap::from([(
                    "question".to_owned(),
                    "Ship {{job}} to {{machine}}?".to_owned(),
                )]),
            },
            Operation::Notify {
                title: Some("Moved".to_owned()),
                body: Some("Now on {{machine}}".to_owned()),
            },
            Operation::Prompt {
                text: "Carry on on {{machine}}".to_owned(),
            },
            Operation::ConfigSet {
                key: "effort".to_owned(),
                value: "{{effort}}".to_owned(),
            },
        ],
    )
}

#[tokio::test]
async fn each_operation_calls_the_session_with_its_arguments_filled_in() {
    let host = Recorder::new(every_operation());

    let finished = run(&host, "test", "go", arguments(&[("machine", "gpu")]))
        .await
        .unwrap();

    assert_eq!(
        host.calls(),
        vec![
            "session.describe: ",
            "session.export: Some(\"out/gpu.bundle\")",
            "session.move: gpu",
            "job.start: web",
            "job.stop: web",
            "monitor.start: watch-web",
            "monitor.stop: watch-web",
            "skill.run: council {\"question\": \"Ship web to gpu?\"}",
            "notify: Some(\"Moved\") Some(\"Now on gpu\")",
            "prompt: Carry on on gpu",
            "config.set: effort=high",
        ]
    );
    let described = StepResult::SessionDescribed {
        description: Box::new(host.describe().await),
    };
    assert_eq!(finished.plugin, "test");
    assert_eq!(finished.command, "go");
    assert_eq!(
        finished.steps,
        vec![
            described,
            StepResult::SessionExported {
                path: PathBuf::from("out/gpu.bundle"),
                conversation_id: "conv-1".to_owned(),
                bytes: 12,
            },
            StepResult::SessionMoveRequested {
                machine: "gpu".to_owned()
            },
            StepResult::JobStarted {
                job: "job-1".to_owned(),
                label: "web".to_owned(),
                state: "running".to_owned(),
            },
            StepResult::JobStopped {
                job: "job-1".to_owned(),
                label: "web".to_owned(),
                state: "killed".to_owned(),
            },
            StepResult::MonitorStarted {
                monitor: "watch-web".to_owned(),
                armed: vec!["watch-web:1".to_owned(), "watch-web:2".to_owned()],
            },
            StepResult::MonitorStopped {
                monitor: "watch-web".to_owned(),
                stopped: vec!["watch-web".to_owned()],
            },
            StepResult::SkillRan {
                skill: "council".to_owned(),
                journal: "/runs/1/journal".to_owned(),
                value: serde_json::json!("decided"),
            },
            StepResult::Notified {
                headline: "Moved".to_owned()
            },
            StepResult::PromptQueued {
                text: "Carry on on gpu".to_owned()
            },
            StepResult::ConfigSet {
                key: "effort".to_owned(),
                value: "high".to_owned(),
            },
        ]
    );
}

#[tokio::test]
async fn a_given_argument_replaces_the_default() {
    let host = Recorder::new(every_operation());

    run(
        &host,
        "test",
        "go",
        arguments(&[("machine", "gpu"), ("job", "api"), ("effort", "low")]),
    )
    .await
    .unwrap();

    let calls = host.calls();
    assert!(calls.contains(&"job.start: api".to_owned()), "{calls:?}");
    assert!(
        calls.contains(&"config.set: effort=low".to_owned()),
        "{calls:?}"
    );
}

#[tokio::test]
async fn a_missing_required_argument_is_named_and_nothing_runs() {
    let host = Recorder::new(every_operation());

    let error = run(&host, "test", "go", BTreeMap::new()).await.unwrap_err();

    assert_eq!(
        error,
        PluginError::MissingArgument {
            plugin: "test".to_owned(),
            command: "go".to_owned(),
            name: "machine".to_owned(),
        }
    );
    assert_eq!(error.to_string(), "`test go` needs the argument `machine`");
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn an_unknown_argument_is_named_with_the_known_ones_and_nothing_runs() {
    let host = Recorder::new(every_operation());

    let error = run(
        &host,
        "test",
        "go",
        arguments(&[("machine", "gpu"), ("machin", "typo")]),
    )
    .await
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "`test go` has no argument `machin`; its arguments are: effort, job, machine"
    );
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn a_placeholder_that_is_no_argument_stops_before_anything_runs() {
    let host = Recorder::new(plugin(
        &[],
        vec![
            Operation::SessionDescribe {},
            Operation::Prompt {
                text: "go to {{where}}".to_owned(),
            },
        ],
    ));

    let error = run(&host, "test", "go", BTreeMap::new()).await.unwrap_err();

    assert_eq!(
        error,
        PluginError::UnknownPlaceholder {
            at: "commands.go.operations[1] (prompt)".to_owned(),
            name: "where".to_owned(),
        }
    );
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn the_first_failure_stops_the_command_and_names_the_step() {
    let host = Recorder::new(plugin(
        &[("machine", parameter(None))],
        vec![
            Operation::JobStop {
                job: "web".to_owned(),
            },
            Operation::SessionMove {
                machine: "{{machine}}".to_owned(),
            },
            Operation::Notify {
                title: None,
                body: Some("never".to_owned()),
            },
        ],
    ))
    .failing("session.move");

    let error = run(&host, "test", "go", arguments(&[("machine", "gpu")]))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        PluginError::StepFailed {
            at: "commands.go.operations[1] (session.move)".to_owned(),
            message: "refused".to_owned(),
            completed: vec![StepResult::JobStopped {
                job: "job-1".to_owned(),
                label: "web".to_owned(),
                state: "killed".to_owned(),
            }],
        }
    );
    assert_eq!(
        error.to_string(),
        "commands.go.operations[1] (session.move): refused"
    );
    assert_eq!(
        host.calls(),
        vec!["job.stop: web".to_owned(), "session.move: gpu".to_owned()]
    );
}

#[tokio::test]
async fn an_empty_prompt_or_machine_fails_its_step() {
    let host = Recorder::new(plugin(
        &[("text", parameter(Some("")))],
        vec![Operation::Prompt {
            text: "{{text}}".to_owned(),
        }],
    ));

    let error = run(&host, "test", "go", BTreeMap::new()).await.unwrap_err();

    assert_eq!(
        error.to_string(),
        "commands.go.operations[0] (prompt): the prompt is empty once its arguments are filled in"
    );
    assert_eq!(host.calls(), Vec::<String>::new());
}

#[tokio::test]
async fn an_unknown_plugin_or_command_lists_the_known_ones() {
    let host = Recorder::new(every_operation());

    let plugin_error = run(&host, "rex", "go", BTreeMap::new()).await.unwrap_err();
    let command_error = run(&host, "test", "stop", BTreeMap::new())
        .await
        .unwrap_err();
    let empty = Recorder {
        catalog: Arc::default(),
        calls: Mutex::default(),
        failing: None,
    };
    let none_error = run(&empty, "rex", "go", BTreeMap::new()).await.unwrap_err();

    assert_eq!(
        plugin_error.to_string(),
        "there is no plugin named `rex`; the plugins are: test"
    );
    assert_eq!(
        command_error.to_string(),
        "plugin `test` has no command `stop`; its commands are: go"
    );
    assert_eq!(
        none_error.to_string(),
        "there is no plugin named `rex`; the plugins are: none are loaded"
    );
}

#[test]
fn plugins_are_listed_with_their_commands_and_parameters() {
    let host = Recorder::new(every_operation());

    assert_eq!(
        PluginInfo::of(&host.catalog),
        vec![PluginInfo {
            name: "test".to_owned(),
            description: "a plugin to test with".to_owned(),
            commands: vec![CommandInfo {
                name: "go".to_owned(),
                description: "go".to_owned(),
                parameters: vec![
                    ParameterInfo {
                        name: "effort".to_owned(),
                        description: "a parameter".to_owned(),
                        required: false,
                        default: Some("high".to_owned()),
                    },
                    ParameterInfo {
                        name: "job".to_owned(),
                        description: "a parameter".to_owned(),
                        required: false,
                        default: Some("web".to_owned()),
                    },
                    ParameterInfo {
                        name: "machine".to_owned(),
                        description: "a parameter".to_owned(),
                        required: true,
                        default: None,
                    },
                ],
            }],
        }]
    );
}

#[test]
fn results_and_errors_cross_the_wire_by_their_operation_names() {
    let step = StepResult::SessionMoveRequested {
        machine: "gpu".to_owned(),
    };
    let error = PluginError::MissingArgument {
        plugin: "rex".to_owned(),
        command: "move".to_owned(),
        name: "machine".to_owned(),
    };

    assert_eq!(
        serde_json::to_value(&step).unwrap(),
        serde_json::json!({"kind": "session.move", "machine": "gpu"})
    );
    assert_eq!(
        serde_json::from_value::<StepResult>(serde_json::to_value(&step).unwrap()).unwrap(),
        step
    );
    assert_eq!(
        serde_json::from_value::<PluginError>(serde_json::to_value(&error).unwrap()).unwrap(),
        error
    );
}
