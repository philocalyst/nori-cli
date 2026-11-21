use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use pretty_assertions::assert_eq;

use super::Catalog;
use super::CatalogCache;
use super::Kind;
use super::Problem;
use super::Roots;
use super::Scope;
use super::load;
use super::watch;
use crate::EvaluatorOptions;
use crate::kinds::Action;
use crate::kinds::ChoiceFrom;
use crate::kinds::Command;
use crate::kinds::CommandCategory;
use crate::kinds::CommandEffect;
use crate::kinds::CommandRequires;
use crate::kinds::JobDef;
use crate::kinds::JobEvent;
use crate::kinds::JobWake;
use crate::kinds::MonitorDef;
use crate::kinds::Operation;
use crate::kinds::Parameter;
use crate::kinds::PluginDef;
use crate::kinds::Ready;
use crate::kinds::Restart;
use crate::kinds::RestartOn;
use crate::kinds::SkillMeta;
use crate::kinds::SkillParameter;
use crate::kinds::Trigger;
use crate::kinds::WakeMode;

/// A stand-in for conduit's `nori:Skill`: the catalog's fields plus a program it must ignore.
const SKILL_SCHEMA: &str = r#"
open module nori.Skill

name: String
summary: String = ""
parameters: Mapping<String, Param>
command: String?
steps: Listing<String>

class Param {
  description: String = ""
}
"#;

fn options() -> EvaluatorOptions {
    EvaluatorOptions::preconfigured().virtual_module("nori:Skill", SKILL_SCHEMA)
}

fn write(root: &Path, relative: &str, content: &str) -> PathBuf {
    let path = root.join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();
    path
}

fn job_source(name: &str, command: &str) -> String {
    format!("amends \"nori:Job\"\nname = \"{name}\"\ncommand = \"{command}\"\n")
}

fn problem_messages(catalog: &Catalog) -> Vec<String> {
    catalog
        .problems
        .iter()
        .map(|problem| problem.message.clone())
        .collect()
}

fn default_job(name: &str, command: &str) -> JobDef {
    JobDef {
        name: name.to_owned(),
        description: String::new(),
        command: command.to_owned(),
        machine: "local".to_owned(),
        cwd: None,
        env: BTreeMap::new(),
        ready: None,
        restart: Restart {
            on: RestartOn::Never,
            max: 3,
            backoff: Duration::from_secs(2),
        },
        wake: JobWake {
            on_exit: true,
            on_ready: false,
            on_output: None,
            on_lost: true,
        },
        check: None,
        max_log_bytes: 67_108_864,
        timeout: None,
        stop_grace: Duration::from_secs(5),
    }
}

#[test]
fn kinds_name_their_directory_and_schema() {
    let names: Vec<_> = Kind::ALL
        .iter()
        .map(|kind| (kind.dir(), kind.schema()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("jobs", "nori:Job"),
            ("monitors", "nori:Monitor"),
            ("skills", "nori:Skill"),
            ("plugins", "nori:Plugin"),
            ("gates", "nori:Gate"),
        ]
    );
}

#[test]
fn a_project_job_shadows_a_user_job_of_the_same_name() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(user.path(), "jobs/web.pkl", &job_source("web", "user-web"));
    write(user.path(), "jobs/db.pkl", &job_source("db", "user-db"));
    let project_web = write(
        project.path(),
        "jobs/dev.pkl",
        &job_source("web", "project-web"),
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: Some(project.path().to_owned()),
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, vec![]);
    let names: Vec<_> = catalog
        .jobs
        .iter()
        .map(|job| (job.name.as_str(), job.scope, job.value.command.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![
            ("db", Scope::User, "user-db"),
            ("web", Scope::Project, "project-web"),
        ]
    );
    assert_eq!(catalog.job("web").unwrap().path, project_web);
    assert_eq!(
        catalog.job("db").unwrap().value,
        default_job("db", "user-db")
    );
    assert_eq!(catalog.job("nope"), None);
}

#[test]
fn a_monitor_decodes_to_the_exact_rust_value() {
    let user = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "monitors/ci-watch.pkl",
        r#"
amends "nori:Monitor"

name = "ci-watch"
description = "Watch the deploy"
on {
  new Every { interval = 90.s }
  new Job { job = "web"; on { "ready"; "failed" } }
  new Cron { expression = "0 3 * * 1-5" }
}
filter = "(?i)error"
actions {
  new RunJob { job = "deps-audit" }
  new RunSkill { skill = "triage"; arguments { ["scope"] = "all" } }
}
wake = "digest"
debounce = 2.s
max_fires = 5
expires = 1.h
"#,
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, vec![]);
    assert_eq!(
        catalog.monitor("ci-watch").unwrap().value,
        MonitorDef {
            name: "ci-watch".to_owned(),
            description: "Watch the deploy".to_owned(),
            on: vec![
                Trigger::Every {
                    interval: Duration::from_secs(90)
                },
                Trigger::Job {
                    job: "web".to_owned(),
                    on: vec![JobEvent::Ready, JobEvent::Failed],
                },
                Trigger::Cron {
                    expression: "0 3 * * 1-5".to_owned()
                },
            ],
            filter: Some("(?i)error".to_owned()),
            fail_on: vec![],
            until: None,
            actions: vec![
                Action::RunJob {
                    job: "deps-audit".to_owned()
                },
                Action::RunSkill {
                    skill: "triage".to_owned(),
                    arguments: BTreeMap::from([("scope".to_owned(), "all".to_owned())]),
                },
            ],
            wake: WakeMode::Digest,
            digest: Duration::from_secs(30),
            debounce: Some(Duration::from_secs(2)),
            max_fires: Some(5),
            expires: Some(Duration::from_secs(3600)),
        }
    );
}

#[test]
fn a_plugin_operation_naming_an_undeclared_parameter_is_a_problem() {
    let user = tempfile::tempdir().unwrap();
    let path = write(
        user.path(),
        "plugins/move.pkl",
        r#"
amends "nori:Plugin"

name = "move"
commands {
  ["to"] {
    parameters { ["machine"] {} }
    operations {
      new SessionMove { machine = "{{machin}}" }
    }
  }
}
"#,
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.plugins, vec![]);
    assert_eq!(catalog.problems.len(), 1);
    assert_eq!(catalog.problems[0].path, path);
    let message = &catalog.problems[0].message;
    assert!(message.contains("{{machin}}"), "{message}");
    assert!(message.contains("declared: machine"), "{message}");
    assert!(
        message.contains("operations[0].machine"),
        "names the field: {message}"
    );
}

#[test]
fn a_plugin_commands_palette_fields_are_checked() {
    let user = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "plugins/loud.pkl",
        r#"
amends "nori:Plugin"

name = "loud"
commands {
  ["say"] {
    title = " "
    names { "Say It" }
    keywords { "ok"; "" }
    parameters {
      ["tone"] { choices { "soft"; "soft" } }
      ["who"] { choices { "me" }; from = "machines" }
    }
    operations { new NotifyOp { body = "{{tone}} {{who}}" } }
  }
}
"#,
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.plugins, vec![]);
    let message = &catalog.problems[0].message;
    for expected in [
        r#"commands["say"].title: must not be empty"#,
        r#"commands["say"].names[0]: "Say It" is not a valid name"#,
        r#"commands["say"].keywords[1]: must not be empty"#,
        r#"parameters["tone"].choices[1]: "soft" is listed twice"#,
        r#"parameters["who"]: give `choices` or `from`, not both"#,
    ] {
        assert!(message.contains(expected), "{expected}\n{message}");
    }
}

#[test]
fn a_typo_field_is_a_problem_with_a_line_number() {
    let user = tempfile::tempdir().unwrap();
    let path = write(
        user.path(),
        "jobs/dev.pkl",
        "amends \"nori:Job\"\n\nname = \"dev\"\ncomand = \"pnpm dev\"\ncommand = \"pnpm dev\"\n",
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.jobs, vec![]);
    assert_eq!(catalog.problems.len(), 1);
    let message = &catalog.problems[0].message;
    assert!(message.contains("comand"), "{message}");
    assert!(
        message.contains(&format!("{}:4", path.display())),
        "{message}"
    );
}

#[test]
fn wrong_or_missing_schemas_say_what_to_amend() {
    let user = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "jobs/a.pkl",
        "amends \"nori:Monitor\"\nname = \"a\"\n",
    );
    write(user.path(), "jobs/b.pkl", "name = \"b\"\ncommand = \"x\"\n");
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(
        problem_messages(&catalog),
        vec![
            "jobs/a.pkl must amend \"nori:Job\", not \"nori:Monitor\"".to_owned(),
            "jobs/b.pkl must amend \"nori:Job\"".to_owned(),
        ]
    );
}

#[test]
fn two_modules_of_one_name_in_one_root_are_a_problem() {
    let user = tempfile::tempdir().unwrap();
    let first = write(user.path(), "jobs/a.pkl", &job_source("web", "one"));
    write(user.path(), "jobs/b.pkl", &job_source("web", "two"));
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.jobs.len(), 1);
    assert_eq!(catalog.jobs[0].value.command, "one");
    assert_eq!(
        problem_messages(&catalog),
        vec![format!(
            "jobs/b.pkl: name \"web\" is already defined by {}",
            first.display()
        )]
    );
}

#[test]
fn a_skill_is_read_without_understanding_its_program() {
    let user = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "skills/council.pkl",
        r#"
amends "nori:Skill"

name = "council"
summary = "Several models review a question."
parameters { ["question"] { description = "What to decide" } }
steps { "ask"; "reconcile" }
"#,
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, vec![]);
    assert_eq!(
        catalog.skill("council").unwrap().value,
        SkillMeta {
            name: "council".to_owned(),
            summary: "Several models review a question.".to_owned(),
            parameters: BTreeMap::from([(
                "question".to_owned(),
                SkillParameter {
                    description: "What to decide".to_owned(),
                    default: None,
                    required: true,
                }
            )]),
            command: None,
        }
    );
}

#[test]
fn an_unchanged_reload_does_not_evaluate_again() {
    let user = tempfile::tempdir().unwrap();
    let path = write(user.path(), "jobs/web.pkl", &job_source("web", "one"));
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };
    let options = options();
    let mut cache = CatalogCache::default();

    let (first, changed) = cache.reload(&roots, &options);
    assert_eq!((changed, cache.evaluations()), (true, 1));

    let (second, changed) = cache.reload(&roots, &options);
    assert_eq!((changed, cache.evaluations()), (false, 1));
    assert_eq!(first, second);

    std::fs::write(&path, job_source("web", "two-longer")).unwrap();
    let (third, changed) = cache.reload(&roots, &options);
    assert_eq!((changed, cache.evaluations()), (true, 2));
    assert_eq!(third.job("web").unwrap().value.command, "two-longer");

    std::fs::remove_file(&path).unwrap();
    let (fourth, changed) = cache.reload(&roots, &options);
    assert_eq!((changed, cache.evaluations()), (true, 2));
    assert_eq!(fourth, Catalog::default());
}

#[test]
fn a_broken_edit_keeps_the_last_good_value_and_reports_the_problem() {
    let user = tempfile::tempdir().unwrap();
    let path = write(user.path(), "jobs/web.pkl", &job_source("web", "one"));
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };
    let options = options();
    let mut cache = CatalogCache::default();
    cache.reload(&roots, &options);

    std::fs::write(
        &path,
        "amends \"nori:Job\"\nname = \"web\"\ncomand = \"oops\"\n",
    )
    .unwrap();
    let (catalog, changed) = cache.reload(&roots, &options);

    assert!(changed);
    assert_eq!(catalog.job("web").unwrap().value.command, "one");
    assert_eq!(catalog.problems.len(), 1);
    assert_eq!(catalog.problems[0].path, path);
}

#[test]
fn validation_names_the_field() {
    let mut job = default_job("Bad Name", "run");
    job.ready = Some(Ready {
        pattern: "(unclosed".to_owned(),
        timeout: Duration::from_secs(1),
    });
    job.restart.max = -1;
    let problems = job.validate().unwrap_err();
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems[0].starts_with("name:"), "{problems:?}");
    assert!(problems[1].starts_with("ready.pattern:"), "{problems:?}");
    assert!(problems[2].starts_with("restart.max:"), "{problems:?}");

    let monitor = MonitorDef {
        name: "m".to_owned(),
        description: String::new(),
        on: vec![
            Trigger::Cron {
                expression: "* * * *".to_owned(),
            },
            Trigger::Cron {
                expression: "0 */5 * * * *".to_owned(),
            },
            Trigger::At {
                time: "17:30".to_owned(),
            },
            Trigger::At {
                time: "2026-12-01T09:00:00+01:00".to_owned(),
            },
            Trigger::At {
                time: "tomorrow".to_owned(),
            },
        ],
        filter: None,
        fail_on: vec![],
        until: Some("[".to_owned()),
        actions: vec![],
        wake: WakeMode::Immediate,
        digest: Duration::from_secs(30),
        debounce: None,
        max_fires: None,
        expires: None,
    };
    let problems = monitor.validate().unwrap_err();
    assert_eq!(problems.len(), 3, "{problems:?}");
    assert!(problems[0].starts_with("on[0].expression:"), "{problems:?}");
    assert!(problems[1].starts_with("on[4].time:"), "{problems:?}");
    assert!(problems[2].starts_with("until:"), "{problems:?}");

    let empty = MonitorDef {
        on: vec![],
        until: None,
        ..monitor
    };
    assert_eq!(
        empty.validate(),
        Err(vec!["on: a monitor needs at least one trigger".to_owned()])
    );
}

#[test]
fn the_example_modules_load() {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let roots = Roots {
        user: Some(examples),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, Vec::<Problem>::new());
    assert_eq!(
        catalog.job("web").unwrap().value,
        JobDef {
            description: "The dev server on :5173".to_owned(),
            cwd: Some("web".to_owned()),
            env: BTreeMap::from([("NODE_ENV".to_owned(), "development".to_owned())]),
            ready: Some(Ready {
                pattern: "Local:.*5173".to_owned(),
                timeout: Duration::from_secs(60),
            }),
            restart: Restart {
                on: RestartOn::Failure,
                max: 3,
                backoff: Duration::from_secs(2),
            },
            wake: JobWake {
                on_exit: true,
                on_ready: true,
                on_output: Some("(?i)error|panic".to_owned()),
                on_lost: true,
            },
            check: Some("curl -fsS http://localhost:5173/ > /dev/null".to_owned()),
            ..default_job("web", "pnpm dev")
        }
    );
    let nightly = &catalog.monitor("nightly-deps").unwrap().value;
    assert_eq!(nightly.on.len(), 3);
    assert_eq!(nightly.wake, WakeMode::Digest);
    assert_eq!(nightly.digest, Duration::from_secs(300));
    assert_eq!(
        catalog.plugin("move").unwrap().value,
        PluginDef {
            name: "move".to_owned(),
            description: "Move the session between machines".to_owned(),
            commands: BTreeMap::from([(
                "to".to_owned(),
                Command {
                    description: "Move this session to another machine".to_owned(),
                    title: None,
                    names: Vec::new(),
                    keywords: Vec::new(),
                    category: CommandCategory::Plugin,
                    effect: CommandEffect::Now,
                    requires: CommandRequires::default(),
                    parameters: BTreeMap::from([(
                        "machine".to_owned(),
                        Parameter {
                            description: "A machine from the roster".to_owned(),
                            default: None,
                            required: true,
                            choices: Vec::new(),
                            from: None,
                        }
                    )]),
                    operations: vec![
                        Operation::SessionMove {
                            machine: "{{machine}}".to_owned()
                        },
                        Operation::Notify {
                            title: Some("Session moved".to_owned()),
                            body: Some("Now running on {{machine}}".to_owned()),
                        },
                    ],
                }
            )]),
        }
    );
}

/// The Rex plugin, alone: it loads whatever else the examples directory holds.
#[test]
fn the_rex_plugin_example_loads() {
    let user = tempfile::tempdir().unwrap();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/plugins/rex.pkl");
    write(
        user.path(),
        "plugins/rex.pkl",
        &std::fs::read_to_string(example).unwrap(),
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, Vec::<Problem>::new());
    let rex = &catalog.plugin("rex").unwrap().value;
    assert_eq!(rex.validate(), Ok(()));
    assert_eq!(
        rex.commands.keys().map(String::as_str).collect::<Vec<_>>(),
        ["move", "park", "where"]
    );
    let moving = &rex.commands["move"];
    assert_eq!(
        moving.title.as_deref(),
        Some("Move session to another machine")
    );
    assert_eq!(moving.category, CommandCategory::Machine);
    assert_eq!(moving.keywords, ["remote", "ssh", "transfer", "rex"]);
    assert_eq!(
        moving.parameters["machine"].from,
        Some(ChoiceFrom::Machines)
    );
    assert_eq!(rex.commands["where"].category, CommandCategory::Session);
    assert_eq!(rex.commands["park"].category, CommandCategory::Tools);
    assert_eq!(
        rex.commands["move"].operations,
        vec![Operation::SessionMove {
            machine: "{{machine}}".to_owned()
        }]
    );
    assert_eq!(
        rex.commands["where"].operations,
        vec![Operation::SessionDescribe {}]
    );
    assert_eq!(
        rex.commands["park"].operations,
        vec![
            Operation::JobStop {
                job: "web".to_owned()
            },
            Operation::Notify {
                title: Some("Parked".to_owned()),
                body: Some("The dev server is stopped".to_owned()),
            },
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn watch_publishes_the_first_load_and_then_only_changes() {
    let user = tempfile::tempdir().unwrap();
    write(user.path(), "jobs/web.pkl", &job_source("web", "one"));
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let (mut receiver, handle) = watch(roots, options(), Duration::from_millis(20));

    tokio::time::timeout(Duration::from_secs(20), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        receiver
            .borrow_and_update()
            .job("web")
            .unwrap()
            .value
            .command,
        "one"
    );

    write(user.path(), "jobs/api.pkl", &job_source("api", "serve"));
    tokio::time::timeout(Duration::from_secs(20), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    let names: Vec<_> = receiver
        .borrow_and_update()
        .jobs
        .iter()
        .map(|job| job.name.clone())
        .collect();
    assert_eq!(names, vec!["api".to_owned(), "web".to_owned()]);

    drop(receiver);
    tokio::time::timeout(Duration::from_secs(20), handle)
        .await
        .unwrap()
        .unwrap();
}

#[test]
fn every_trigger_action_and_operation_decodes_and_round_trips() {
    let user = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "monitors/all.pkl",
        r#"
amends "nori:Monitor"

name = "all"
on {
  new Every { interval = 1.min }
  new At { time = "17:30" }
  new Cron { expression = "*/5 * * * * *" }
  new Idle { after = 10.min }
  new Session { events { "turn_ended"; "quota_low" } }
  new Job { job = "web"; on { "output" } }
  new Output { command = "tail -f log"; stderr = true }
  new File { path = "a.log"; from_start = true }
  new Watch { paths { "src" }; kinds { "created"; "deleted" } }
  new Http { url = "http://x"; json_path = "a.b"; regex = "v[0-9]+" }
  new Ci { branch = "main" }
  new Actor { actor = "reviewer"; track { "results" } }
}
fail_on { "FAIL" }
until = "done"
actions {
  new Wake { message = "hi {{event}}" }
  new Prompt { text = "go" }
  new Notify { title = "t"; body = "b" }
  new StopJob { job = "web" }
}
"#,
    );
    write(
        user.path(),
        "plugins/all.pkl",
        r#"
amends "nori:Plugin"

name = "all"
commands {
  ["run"] {
    parameters { ["x"] { default = "1"; required = false } }
    operations {
      new SessionDescribe {}
      new SessionExport { path = "{{x}}.json" }
      new JobStart { job = "web" }
      new JobStop { job = "web" }
      new MonitorStart { monitor = "all" }
      new MonitorStop { monitor = "all" }
      new SkillRun { skill = "council"; arguments { ["q"] = "{{x}}" } }
      new PromptOp { text = "go {{x}}" }
      new ConfigSet { key = "model"; value = "opus" }
    }
  }
}
"#,
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: None,
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, vec![]);
    let monitor = &catalog.monitor("all").unwrap().value;
    assert_eq!(monitor.on.len(), 12);
    assert_eq!(monitor.actions.len(), 4);
    let json = serde_json::to_value(monitor).unwrap();
    assert_eq!(
        &serde_json::from_value::<MonitorDef>(json).unwrap(),
        monitor
    );
    let plugin = &catalog.plugin("all").unwrap().value;
    assert_eq!(plugin.commands["run"].operations.len(), 9);
    assert_eq!(
        plugin.commands["run"].operations[0],
        Operation::SessionDescribe {}
    );
    let json = serde_json::to_value(plugin).unwrap();
    assert_eq!(&serde_json::from_value::<PluginDef>(json).unwrap(), plugin);
}

#[test]
fn unknown_fields_in_bound_types_are_rejected() {
    let job = serde_json::json!({"name": "a", "command": "b", "bogus": 1});
    assert!(serde_json::from_value::<JobDef>(job).is_err());
    let operation = serde_json::json!({"kind": "session.describe", "extra": 1});
    assert!(serde_json::from_value::<Operation>(operation).is_err());
    let trigger = serde_json::json!({"kind": "ci", "branch": null, "extra": 1});
    assert!(serde_json::from_value::<Trigger>(trigger).is_err());
}

#[test]
fn built_in_modules_sit_beneath_the_users_and_the_projects() {
    let user = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(
        user.path(),
        "builtin/jobs/web.pkl",
        &job_source("web", "builtin-web"),
    );
    write(
        user.path(),
        "builtin/jobs/db.pkl",
        &job_source("db", "builtin-db"),
    );
    write(
        user.path(),
        "builtin/jobs/ci.pkl",
        &job_source("ci", "builtin-ci"),
    );
    write(user.path(), "jobs/web.pkl", &job_source("web", "user-web"));
    write(
        project.path(),
        "jobs/db.pkl",
        &job_source("db", "project-db"),
    );
    let roots = Roots {
        user: Some(user.path().to_owned()),
        project: Some(project.path().to_owned()),
    };

    let catalog = load(&roots, &options());

    assert_eq!(catalog.problems, vec![]);
    let jobs: Vec<_> = catalog
        .jobs
        .iter()
        .map(|job| (job.name.as_str(), job.scope, job.value.command.as_str()))
        .collect();
    assert_eq!(
        jobs,
        vec![
            ("ci", Scope::Builtin, "builtin-ci"),
            ("db", Scope::Project, "project-db"),
            ("web", Scope::User, "user-web"),
        ]
    );
}

#[test]
fn a_default_that_is_not_one_of_the_choices_is_a_problem() {
    let parameter = Parameter {
        description: String::new(),
        default: Some("shout".to_owned()),
        required: true,
        choices: vec!["soft".to_owned(), "loud".to_owned()],
        from: None,
    };
    let plugin = PluginDef {
        name: "loud".to_owned(),
        description: String::new(),
        commands: BTreeMap::from([(
            "say".to_owned(),
            Command {
                description: String::new(),
                title: None,
                names: Vec::new(),
                keywords: Vec::new(),
                category: CommandCategory::Plugin,
                effect: CommandEffect::Now,
                requires: CommandRequires::default(),
                parameters: BTreeMap::from([("tone".to_owned(), parameter)]),
                operations: Vec::new(),
            },
        )]),
    };

    assert_eq!(
        plugin.validate(),
        Err(vec![
            r#"commands["say"].parameters["tone"].default: "shout" is not one of the choices (soft, loud)"#
                .to_owned()
        ])
    );
}
