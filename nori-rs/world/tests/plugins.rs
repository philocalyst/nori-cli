//! Plugins (`nori:Plugin`) through the real harness: a module in `$NORI_HOME/plugins/` runs its
//! operations on the session, and every operation does what the session already does for its
//! tools and its person.
//!
//! World: `fake-agent/worlds/plugins.pkl`. The plugin and the job module it starts are written into
//! the session's home before it boots (`WorldOptions::configure`).

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use nori_actions::Action;
use nori_actions::ActionId;
use nori_actions::Category;
use nori_actions::ChoiceSource;
use nori_actions::Effect;
use nori_actions::Invocation;
use nori_actions::Param;
use nori_actions::Requires;
use nori_actions::Source;
use nori_config::AskForApproval;
use nori_harness::backend::plugins::PluginError;
use nori_harness::backend::plugins::StepResult;
use nori_harness::backend::plugins::remote;
use nori_protocol::NoriEvent;
use nori_world::WorldSession;
use pretty_assertions::assert_eq;

const WORLD: &str = "plugins.pkl";

const DEMO: &str = r#"amends "nori:Plugin"

name = "demo"
description = "Commands the world test runs"
commands {
  ["look"] {
    description = "Describe the session"
    operations { new SessionDescribe {} }
  }
  ["serve"] {
    description = "Start the dev server"
    operations { new JobStart { job = "dev" } }
  }
  ["halt"] {
    description = "Stop the dev server"
    operations { new JobStop { job = "dev" } }
  }
  ["say"] {
    description = "Tell the person something"
    parameters { ["text"] { description = "What to say" } }
    operations { new NotifyOp { title = "Demo"; body = "{{text}}" } }
  }
  ["ask"] {
    description = "Queue a prompt"
    parameters { ["text"] { description = "The prompt" } }
    operations { new PromptOp { text = "{{text}}" } }
  }
  ["set"] {
    description = "Change a session setting"
    parameters {
      ["key"] { description = "The setting" }
      ["value"] { description = "Its new value" }
    }
    operations { new ConfigSet { key = "{{key}}"; value = "{{value}}" } }
  }
  ["go"] {
    description = "Move the session"
    parameters { ["machine"] { description = "A machine from the roster" } }
    operations {
      new SessionMove { machine = "{{machine}}" }
      new NotifyOp { body = "never reached when the move is refused" }
    }
  }
  ["round"] {
    description = "Several operations in order"
    operations {
      new JobStart { job = "dev" }
      new NotifyOp { title = "Demo"; body = "started" }
      new JobStop { job = "dev" }
    }
  }
}
"#;

/// The Rex plugin as shipped.
const REX: &str = include_str!("../../pkl/examples/plugins/rex.pkl");

const DEV: &str = r#"amends "nori:Job"

name = "dev"
description = "The scripted dev server"
command = "serve-plugin-demo"
"#;

async fn boot() -> WorldSession {
    let mut options = support::options("lead");
    options.configure = Some(Box::new(|config: &mut nori_config::NoriConfig| {
        for (directory, name, source) in [
            ("plugins", "demo.pkl", DEMO),
            ("plugins", "rex.pkl", REX),
            ("jobs", "dev.pkl", DEV),
        ] {
            let directory = config.nori_home.join(directory);
            std::fs::create_dir_all(&directory).expect("a module directory");
            std::fs::write(directory.join(name), source).expect("a module");
        }
    }));
    WorldSession::boot(support::world(WORLD), options)
        .await
        .unwrap_or_else(|error| panic!("boot: {error:#}"))
}

fn args(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect()
}

fn notices(session: &WorldSession) -> Vec<String> {
    session
        .events()
        .nori()
        .filter_map(|event| match event {
            NoriEvent::Notice(notice) => Some(notice.message.clone()),
            _ => None,
        })
        .collect()
}

/// The session's own picture of itself, through the `look` command.
async fn look(session: &WorldSession) -> nori_harness::backend::plugins::SessionDescription {
    let run = session
        .handle()
        .run_plugin("demo", "look", BTreeMap::new())
        .await
        .expect("the look command runs");
    let [StepResult::SessionDescribed { description }] = run.steps.as_slice() else {
        panic!("one description: {:?}", run.steps);
    };
    (**description).clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_offers_the_plugins_its_modules_declare() {
    let session = boot().await;

    let plugins = session.handle().plugins().await.expect("the plugin list");

    let names: Vec<&str> = plugins.iter().map(|plugin| plugin.name.as_str()).collect();
    assert_eq!(names, ["demo", "rex"]);
    let plugins: Vec<_> = plugins
        .iter()
        .filter(|plugin| plugin.name == "demo")
        .collect();
    assert_eq!(plugins[0].description, "Commands the world test runs");
    let commands: Vec<&str> = plugins[0]
        .commands
        .iter()
        .map(|command| command.name.as_str())
        .collect();
    assert_eq!(
        commands,
        ["ask", "go", "halt", "look", "round", "say", "serve", "set"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn describing_the_session_reports_what_it_is() {
    let session = boot().await;

    let description = look(&session).await;

    assert_eq!(description.cwd, session.cwd());
    assert_eq!(
        description.permissions.approval_policy,
        AskForApproval::OnRequest
    );
    assert!(!description.conversation_id.is_empty(), "{description:?}");
    assert!(!description.machine.is_empty(), "{description:?}");
    assert_eq!(
        description
            .settings
            .iter()
            .find(|setting| setting.id == "model")
            .map(|setting| setting.value.as_str()),
        Some("fake-sonnet"),
        "{:?}",
        description.settings
    );
    assert!(description.jobs.is_empty() && description.monitors.is_empty());
}

/// A job module starts on a scripted process, shows in the session's description, and stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_plugin_starts_and_stops_a_job_module() {
    let session = boot().await;

    let started = session
        .handle()
        .run_plugin("demo", "serve", BTreeMap::new())
        .await
        .expect("the job starts");
    let [StepResult::JobStarted { label, state, .. }] = started.steps.as_slice() else {
        panic!("one job started: {:?}", started.steps);
    };
    assert_eq!((label.as_str(), state.as_str()), ("dev", "running"));

    let description = look(&session).await;
    assert_eq!(description.jobs.len(), 1, "{:?}", description.jobs);
    assert_eq!(description.jobs[0].label, "dev");
    assert_eq!(description.jobs[0].command, "serve-plugin-demo");

    let stopped = session
        .handle()
        .run_plugin("demo", "halt", BTreeMap::new())
        .await
        .expect("the job stops");
    let [StepResult::JobStopped { label, state, .. }] = stopped.steps.as_slice() else {
        panic!("one job stopped: {:?}", stopped.steps);
    };
    assert_eq!(label, "dev");
    assert_ne!(state, "running", "the job is still running");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn notify_leaves_one_line_in_the_transcript() {
    let session = boot().await;

    let run = session
        .handle()
        .run_plugin("demo", "say", args(&[("text", "hello from a plugin")]))
        .await
        .expect("the command runs");

    assert_eq!(
        run.steps,
        vec![StepResult::Notified {
            headline: "Demo".to_owned()
        }]
    );
    session
        .wait_for("the notice", |_| {
            notices(&session)
                .iter()
                .any(|line| line == "◉ Demo: hello from a plugin")
        })
        .await
        .expect("the person is told");
    assert!(
        notices(&session)
            .iter()
            .any(|line| line == "plugin demo say: 1 step done"),
        "{:?}",
        notices(&session)
    );
}

/// The conversation journal holds the run as one `plugin` command, so a resumed session can see it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_is_journaled_as_one_plugin_command() {
    let session = boot().await;
    session
        .handle()
        .run_plugin("demo", "say", args(&[("text", "journal me")]))
        .await
        .expect("the command runs");
    session
        .handle()
        .flush_transcript()
        .await
        .expect("the journal is written");

    let mut journals = Vec::new();
    let mut directories = vec![session.nori_home().to_owned()];
    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)
            .expect("a directory")
            .flatten()
        {
            let path = entry.path();
            if path.is_dir() {
                directories.push(path);
            } else if path.extension().is_some_and(|extension| extension == "pkl")
                && let Ok(text) = std::fs::read_to_string(&path)
                && text.contains("journal me")
            {
                journals.push(text);
            }
        }
    }
    assert_eq!(journals.len(), 1, "{journals:?}");
    assert_eq!(
        journals[0].matches(r#"{\"name\":\"plugin\""#).count(),
        1,
        "one record per run: {}",
        journals[0]
    );
    assert!(
        journals[0].contains("plugin demo say: 1 step done"),
        "the outcome is in the transcript: {}",
        journals[0]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_queued_prompt_is_answered_by_the_agent() {
    let session = boot().await;

    let run = session
        .handle()
        .run_plugin("demo", "ask", args(&[("text", "ping")]))
        .await
        .expect("the command runs");

    assert_eq!(
        run.steps,
        vec![StepResult::PromptQueued {
            text: "ping".to_owned()
        }]
    );
    session
        .wait_for("the agent's answer", |events| {
            events.root().text().contains("pong")
        })
        .await
        .expect("the agent answers the queued prompt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn config_set_changes_the_model_and_a_setting_the_agent_lacks_is_named() {
    let session = boot().await;

    let run = session
        .handle()
        .run_plugin(
            "demo",
            "set",
            args(&[("key", "model"), ("value", "fake-opus")]),
        )
        .await
        .expect("the model changes");
    let refused = session
        .handle()
        .run_plugin("demo", "set", args(&[("key", "nope"), ("value", "x")]))
        .await
        .expect_err("the agent has no such setting");

    assert_eq!(
        run.steps,
        vec![StepResult::ConfigSet {
            key: "model".to_owned(),
            value: "fake-opus".to_owned()
        }]
    );
    let model = look(&session)
        .await
        .settings
        .into_iter()
        .find(|setting| setting.id == "model")
        .map(|setting| setting.value);
    assert_eq!(model.as_deref(), Some("fake-opus"));
    let PluginError::StepFailed {
        at,
        message,
        completed,
    } = refused
    else {
        panic!("a failed step: {refused:?}");
    };
    assert_eq!(at, "commands.set.operations[0] (config.set)");
    assert!(
        message.starts_with("the agent offers no setting `nope`"),
        "{message}"
    );
    assert_eq!(completed, Vec::<StepResult>::new());
}

/// A move the roster cannot reach fails at its step; a command stops there and later steps do not
/// run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_to_a_machine_nobody_knows_stops_the_command_at_that_step() {
    let session = boot().await;

    let error = session
        .handle()
        .run_plugin("demo", "go", args(&[("machine", "ghost")]))
        .await
        .expect_err("there is no such machine");

    let PluginError::StepFailed {
        at,
        message,
        completed,
    } = error
    else {
        panic!("a failed step: {error:?}");
    };
    assert_eq!(at, "commands.go.operations[0] (session.move)");
    assert!(message.contains("no machine named `ghost`"), "{message}");
    assert_eq!(completed, Vec::<StepResult>::new());
    assert!(
        notices(&session)
            .iter()
            .all(|line| !line.contains("never reached")),
        "{:?}",
        notices(&session)
    );
}

/// The move is the frontend's: the harness asks for it with an event and says so in the result.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_move_is_requested_of_the_frontend_that_owns_the_terminal() {
    let session = boot().await;
    std::fs::write(
        session.nori_home().join("config.toml"),
        "[machines]\nrex_hosts = false\n\n[machines.gpu]\nssh = \"me@gpu.example.com\"\n",
    )
    .expect("a roster with one machine");
    nori_machines::install(Arc::new(nori_machines::Roster::discover(
        session.nori_home(),
    )));

    let run = session
        .handle()
        .run_plugin("demo", "go", args(&[("machine", "gpu")]))
        .await
        .expect("the move is requested");

    assert_eq!(
        run.steps[0],
        StepResult::SessionMoveRequested {
            machine: "gpu".to_owned()
        }
    );
    session
        .wait_for("the request", |events| {
            events.nori().any(|event| {
                matches!(event, NoriEvent::SessionMoveRequested { machine } if machine == "gpu")
            })
        })
        .await
        .expect("the frontend is asked to move");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_runs_its_operations_in_order() {
    let session = boot().await;

    let run = session
        .handle()
        .run_plugin("demo", "round", BTreeMap::new())
        .await
        .expect("the command runs");

    let kinds: Vec<String> = run
        .steps
        .iter()
        .map(|step| {
            serde_json::to_value(step).unwrap()["kind"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(kinds, ["job.start", "notify", "job.stop"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mistakes_in_the_arguments_are_named_before_anything_runs() {
    let session = boot().await;

    let missing = session
        .handle()
        .run_plugin("demo", "say", BTreeMap::new())
        .await
        .expect_err("the text is required");
    let unknown = session
        .handle()
        .run_plugin("demo", "say", args(&[("text", "hi"), ("tone", "loud")]))
        .await
        .expect_err("no such argument");

    assert_eq!(missing.to_string(), "`demo say` needs the argument `text`");
    assert_eq!(
        unknown.to_string(),
        "`demo say` has no argument `tone`; its arguments are: text"
    );
    assert!(
        notices(&session).iter().all(|line| !line.contains("hi")),
        "{:?}",
        notices(&session)
    );
}

/// What a Rex client sees: the two extension methods answer with the same typed values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_remote_control_methods_list_and_run_plugins() {
    let session = boot().await;
    let handle = session.handle();

    let listed = remote::answer(&handle, remote::PLUGINS_METHOD, serde_json::json!({}))
        .await
        .expect("a plugin method")
        .expect("the list");
    let ran = remote::answer(
        &handle,
        remote::PLUGIN_RUN_METHOD,
        serde_json::json!({"plugin": "demo", "command": "say", "arguments": {"text": "hi"}}),
    )
    .await
    .expect("a plugin method")
    .expect("the run");
    let refused = remote::answer(
        &handle,
        remote::PLUGIN_RUN_METHOD,
        serde_json::json!({"plugin": "demo", "command": "say"}),
    )
    .await
    .expect("a plugin method")
    .expect_err("the argument is missing");
    let other = remote::answer(&handle, "_nori/elsewhere", serde_json::json!({})).await;

    assert_eq!(listed["plugins"][0]["name"], "demo");
    assert_eq!(
        ran,
        serde_json::json!({
            "plugin": "demo",
            "command": "say",
            "steps": [{"kind": "notify", "headline": "Demo"}],
        })
    );
    assert_eq!(refused.message, "`demo say` needs the argument `text`");
    assert_eq!(
        refused.data,
        Some(serde_json::json!({
            "error": "MissingArgument",
            "plugin": "demo",
            "command": "say",
            "name": "text",
        }))
    );
    assert!(other.is_none());
}

/// What the TUI lists: every command of every plugin as an action, from the modules as loaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plugin_actions_list_every_command_as_an_action() {
    let session = boot().await;
    let mut actions = session.handle().plugin_actions();

    let loaded = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        actions.wait_for(|actions| !actions.is_empty()),
    )
    .await
    .expect("the modules load")
    .expect("the feed is open")
    .clone();

    let ids: Vec<String> = loaded.iter().map(|action| action.id.to_string()).collect();
    assert_eq!(
        ids,
        [
            "demo.ask",
            "demo.go",
            "demo.halt",
            "demo.look",
            "demo.round",
            "demo.say",
            "demo.serve",
            "demo.set",
            "rex.move",
            "rex.park",
            "rex.where",
        ]
    );
    let moving = loaded
        .iter()
        .find(|action| action.id.as_str() == "rex.move")
        .expect("the Rex move action");
    assert_eq!(
        moving,
        &Action {
            id: ActionId::new("rex.move"),
            title: "Move session to another machine".to_owned(),
            summary: "Move this session to another machine".to_owned(),
            names: vec!["rex-move".to_owned()],
            keywords: ["remote", "ssh", "transfer", "rex"]
                .map(str::to_owned)
                .to_vec(),
            category: Category::Machine,
            param: Param::Choice(ChoiceSource::Machines),
            effect: Effect::Now,
            requires: Requires::default(),
            keys: Vec::new(),
            source: Source::Plugin("rex".to_owned()),
        }
    );
}

/// `/rex-where` and `/demo-set model=...`: the invocation's argument becomes the command's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_action_invocation_runs_its_command() {
    let session = boot().await;
    let handle = session.handle();

    let described = handle
        .run_plugin_action(Invocation::new(&ActionId::new("rex.where"), None))
        .await
        .expect("rex.where runs");
    let set = handle
        .run_plugin_action(Invocation::new(
            &ActionId::new("demo.set"),
            Some("key=model value=fake-opus".to_owned()),
        ))
        .await
        .expect("demo.set runs");
    let moved = handle
        .run_plugin_action(Invocation::new(
            &ActionId::new("demo.go"),
            Some("ghost".to_owned()),
        ))
        .await
        .expect_err("there is no machine named ghost");
    let unknown = handle
        .run_plugin_action(Invocation::new(&ActionId::new("rex.nope"), None))
        .await
        .expect_err("no such action");
    let too_much = handle
        .run_plugin_action(Invocation::new(
            &ActionId::new("rex.where"),
            Some("now".to_owned()),
        ))
        .await
        .expect_err("rex.where takes no argument");

    assert_eq!(described.plugin, "rex");
    assert_eq!(described.command, "where");
    assert!(
        matches!(
            described.steps.as_slice(),
            [StepResult::SessionDescribed { .. }]
        ),
        "{:?}",
        described.steps
    );
    assert_eq!(
        set.steps,
        vec![StepResult::ConfigSet {
            key: "model".to_owned(),
            value: "fake-opus".to_owned()
        }]
    );
    let PluginError::StepFailed { at, message, .. } = moved else {
        panic!("a failed step: {moved:?}");
    };
    assert_eq!(at, "commands.go.operations[0] (session.move)");
    assert!(message.contains("no machine named `ghost`"), "{message}");
    assert!(
        unknown
            .to_string()
            .starts_with("there is no plugin action `rex.nope`; the plugin actions are: demo.ask"),
        "{unknown}"
    );
    assert_eq!(
        too_much.to_string(),
        "`rex where`: it takes no argument, but was given `now`"
    );
}
