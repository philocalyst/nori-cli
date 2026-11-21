//! The unified `/model` picker retries equivalent versioned model routes.
#![cfg(unix)]

use tui_pty_e2e::SessionConfig;
use tui_pty_e2e::TIMEOUT;
use tui_pty_e2e::TuiSession;

const MODEL_VALUE: &str = "provider-a/fixture-1.0";
static ROUTE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn route_log_path() -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "nori-model-route-{}-{}.log",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn journal_paths(session: &TuiSession) -> Vec<std::path::PathBuf> {
    let projects = session
        .nori_home_path()
        .expect("isolated Nori home")
        .join("transcripts/by-project");
    let mut journals: Vec<_> = std::fs::read_dir(projects)
        .unwrap()
        .filter_map(Result::ok)
        .flat_map(|project| {
            std::fs::read_dir(project.path().join("sessions"))
                .into_iter()
                .flatten()
        })
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "pkl"))
        .collect();
    journals.sort();
    journals
}

fn config(route_log: &std::path::Path, failure_kind: &str) -> SessionConfig {
    let refreshed_at: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap();
    let catalog = serde_json::json!({
        "mock-model-alt": {
            "models": [{"value": "provider-b/fixture-1.0", "name": "Fixture 1.0"}],
            "settings": [],
            "refreshed_at": refreshed_at
        }
    });
    SessionConfig::new()
        .with_extra_config_toml(
            r#"
[[agents]]
name = "Disabled Claude"
slug = "claude-code"
[agents.distribution.local]
command = "/usr/bin/false"
[[agents]]
name = "Disabled Codex"
slug = "codex"
[agents.distribution.local]
command = "/usr/bin/false"
[[agents]]
name = "Disabled OpenCode"
slug = "opencode"
[agents.distribution.local]
command = "/usr/bin/false"
"#,
        )
        .with_agent_models_json(catalog.to_string())
        .with_agent_env("MOCK_AGENT_VERSIONED_MODEL", "1")
        .with_agent_env("MOCK_AGENT_MODEL_VALUE", MODEL_VALUE)
        .with_agent_env(
            "MOCK_AGENT_MODEL_VALUE_MOCK_MODEL_ALT",
            "provider-b/fixture-1.0",
        )
        .with_agent_env("MOCK_AGENT_MODEL_DISPLAY", "Fixture 1.0")
        .with_agent_env("MOCK_AGENT_CONFIG_SET_LOG", route_log.display().to_string())
        .with_agent_env("MOCK_AGENT_CONFIG_SET_FAIL_VALUE", MODEL_VALUE)
        .with_agent_env("MOCK_AGENT_CONFIG_SET_FAIL_KIND", failure_kind)
}

fn select_shared_model(session: &mut TuiSession) {
    session.submit_input("/model").expect("open model picker");
    session
        .wait_for_text("Select model", TIMEOUT)
        .expect("model picker");
    session
        .type_input("Fixture 1.0")
        .expect("filter to shared model");
    session
        .wait_for(
            |screen| {
                screen
                    .lines()
                    .any(|line| line.contains('❯') && line.contains("Fixture 1.0"))
            },
            TIMEOUT,
        )
        .expect("full search query rendered");
    let screen = session.screen_contents();
    // The search field and the single filtered result each contain the label.
    assert_eq!(screen.matches("Fixture 1.0").count(), 2, "{screen}");
    session
        .send_key(tui_pty_e2e::Key::Enter)
        .expect("select shared model");
}

#[test]
fn quota_rejection_tries_the_alternate_provider_for_one_versioned_row() {
    let _serial = ROUTE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let log = route_log_path();
    let config = config(&log, "quota").with_agent_env(
        "MOCK_AGENT_RESPONSE_MOCK_MODEL_ALT",
        "Alternate route answered",
    );
    let mut session = TuiSession::spawn_with_config(30, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session
        .submit_input("Warm up model picker")
        .expect("start mock ACP session");
    session
        .wait_for_text("Test message 2", TIMEOUT)
        .expect("initial mock response");
    session
        .wait_for_text("Mock Default M", TIMEOUT)
        .expect("initial model config is ready");
    select_shared_model(&mut session);
    session
        .wait_for_text("Mock ACP Alt", TIMEOUT)
        .expect("alternate provider applied");
    session
        .submit_input("Exercise model route retry")
        .expect("submit prompt");
    session
        .wait_for_text("Alternate route answered", TIMEOUT)
        .expect("prompt answered by alternate provider");

    let attempts = std::fs::read_to_string(&log).expect("first route request was recorded");
    assert_eq!(
        attempts.lines().map(str::to_owned).collect::<Vec<_>>(),
        [
            format!("mock-model\t{MODEL_VALUE}"),
            "mock-model-alt\tprovider-b/fixture-1.0".to_owned(),
        ]
    );
    let _ = std::fs::remove_file(log);
}

#[test]
fn a_rejection_that_is_not_quota_also_tries_the_alternate_provider() {
    let _serial = ROUTE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let log = route_log_path();
    let mut session = TuiSession::spawn_with_config(30, 100, config(&log, "invalid")).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session
        .submit_input("Warm up model picker")
        .expect("start mock ACP session");
    session
        .wait_for_text("Test message 2", TIMEOUT)
        .expect("initial mock response");
    session
        .wait_for_text("Mock Default M", TIMEOUT)
        .expect("initial model config is ready");
    select_shared_model(&mut session);
    // The person chose the model, not a route: whatever keeps one route from running it says
    // nothing about the next (see `model_routes::eligible_error`).
    session
        .wait_for_text("trying Mock ACP Alt", TIMEOUT)
        .expect("the next route is tried");
    session
        .wait_for_text("Mock ACP Alt ·", TIMEOUT)
        .expect("the alternate route runs the model");

    let attempts = std::fs::read_to_string(&log).expect("first route request was recorded");
    assert_eq!(
        attempts.lines().map(str::to_owned).collect::<Vec<_>>(),
        [
            format!("mock-model\t{MODEL_VALUE}"),
            "mock-model-alt\tprovider-b/fixture-1.0".to_owned(),
        ]
    );
    let _ = std::fs::remove_file(log);
}

#[test]
fn codex_quota_returns_to_the_selected_opencode_model_without_config_notices() {
    let _serial = ROUTE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let route_log = route_log_path();
    let prompt_log = route_log.with_extension("prompts.jsonl");
    let mock_agent = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("mock_acp_agent{}", std::env::consts::EXE_SUFFIX));
    let mock_agent = serde_json::to_string(&mock_agent.to_string_lossy()).unwrap();
    let route_log_value = serde_json::to_string(&route_log.to_string_lossy()).unwrap();
    let prompt_log_value = serde_json::to_string(&prompt_log.to_string_lossy()).unwrap();
    let extra_config = format!(
        r#"
[[agents]]
name = "OpenCode fixture"
slug = "opencode"
[agents.distribution.local]
command = {mock_agent}
env = {{ MOCK_AGENT_MODEL_NAME = "opencode", MOCK_AGENT_VERSIONED_MODEL = "1", MOCK_AGENT_MODEL_VALUE = "openai/gpt-6-astra", MOCK_AGENT_MODEL_DISPLAY = "GPT-6 Astra", MOCK_AGENT_CONFIG_SET_LOG = {route_log_value}, MOCK_AGENT_PROMPT_LOG = {prompt_log_value}, MOCK_AGENT_RESPONSE = "OpenCode answered", MOCK_AGENT_RESPONSE_MATCH_TEXT = "warm-up-model-picker", MOCK_AGENT_RESPONSE_MATCH = "Warm-up complete" }}
[agents.model_override]
env = "MOCK_AGENT_INJECTED_MODEL"

[[agents]]
name = "Codex fixture"
slug = "codex"
[agents.distribution.local]
command = {mock_agent}
env = {{ MOCK_AGENT_MODEL_NAME = "codex", MOCK_AGENT_VERSIONED_MODEL = "1", MOCK_AGENT_MODEL_VALUE = "gpt-6-astra", MOCK_AGENT_MODEL_DISPLAY = "GPT-6 Astra", MOCK_AGENT_CONFIG_SET_LOG = {route_log_value}, MOCK_AGENT_PROMPT_LOG = {prompt_log_value}, MOCK_AGENT_EXHAUSTED_MODEL = "codex" }}
[agents.model_override]
env = "MOCK_AGENT_INJECTED_MODEL"

[[agents]]
name = "Disabled Claude"
slug = "claude-code"
[agents.distribution.local]
command = "/usr/bin/false"
"#
    );
    let refreshed_at: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap();
    let catalog = serde_json::json!({
        "codex": {
            "models": [{"value": "gpt-6-astra", "name": "GPT-6 Astra"}],
            "settings": [],
            "refreshed_at": refreshed_at
        }
    });
    let config = SessionConfig::new()
        .with_agent("opencode".to_owned())
        .with_extra_config_toml(extra_config)
        .with_agent_models_json(catalog.to_string());
    let mut session = TuiSession::spawn_with_config(30, 140, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session
        .wait_for_text("OpenCode fixture", TIMEOUT)
        .expect("initial provider session is ready");
    session
        .submit_input("warm-up-model-picker")
        .expect("start OpenCode ACP session");
    session
        .wait_for_text("Warm-up complete", TIMEOUT)
        .expect("initial OpenCode response");
    session
        .wait_for_text("Mock Default M", TIMEOUT)
        .expect("initial model config is ready");
    session.submit_input("/model").expect("open model picker");
    session
        .wait_for_text("Select model", TIMEOUT)
        .expect("model picker");
    session
        .type_input("GPT-6 Astra")
        .expect("filter the shared model");
    let screen = session.screen_contents();
    assert_eq!(screen.matches("GPT-6 Astra").count(), 2, "{screen}");
    session
        .send_key(tui_pty_e2e::Key::Enter)
        .expect("select shared model");
    session
        .wait_for_text("Codex fixture", TIMEOUT)
        .expect("Codex route selected first");
    session
        .wait_for_text("GPT-6 Astra", TIMEOUT)
        .unwrap_or_else(|error| {
            panic!(
                "selected model activated: {error}; routes: {:?}",
                std::fs::read_to_string(&route_log)
            )
        });
    session
        .submit_input("Exercise same-model provider fallback")
        .expect("submit prompt");
    session
        .wait_for_text("OpenCode answered", TIMEOUT)
        .expect("OpenCode route answers after Codex quota exhaustion");

    let routes = std::fs::read_to_string(&route_log).expect("model config route log");
    // The OpenCode route starts on the model (it is handed over with the session), so only the
    // Codex route sets it.
    assert_eq!(
        routes.lines().map(str::to_owned).collect::<Vec<_>>(),
        ["codex\tgpt-6-astra".to_owned()]
    );
    let prompts = std::fs::read_to_string(&prompt_log).expect("prompt route log");
    let prompts: Vec<_> = prompts
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect();
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    assert_eq!(prompts[0]["agent"], "opencode");
    assert!(
        prompts[0]["content"]
            .to_string()
            .contains("warm-up-model-picker")
    );
    assert_eq!(prompts[1]["agent"], "codex");
    assert_eq!(prompts[2]["agent"], "opencode");
    let screen = session.screen_contents();
    assert!(screen.contains("OpenCode fixture"), "{screen}");
    assert!(!screen.contains("option set:"), "{screen}");
    let _ = std::fs::remove_file(route_log);
    let _ = std::fs::remove_file(prompt_log);
}

#[test]
fn manual_model_handoff_keeps_visible_context_journal_and_inflight_draft() {
    let _serial = ROUTE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let route_log = route_log_path();
    let prompt_log = route_log.with_extension("prompts.jsonl");
    let mock_agent = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("mock_acp_agent{}", std::env::consts::EXE_SUFFIX));
    let mock_agent = serde_json::to_string(&mock_agent.to_string_lossy()).unwrap();
    let route_log_value = serde_json::to_string(&route_log.to_string_lossy()).unwrap();
    let prompt_log_value = serde_json::to_string(&prompt_log.to_string_lossy()).unwrap();
    let extra_config = format!(
        r#"
[[agents]]
name = "OpenCode fixture"
slug = "opencode"
[agents.distribution.local]
command = {mock_agent}
env = {{ MOCK_AGENT_MODEL_NAME = "opencode", MOCK_AGENT_VERSIONED_MODEL = "1", MOCK_AGENT_MODEL_VALUE = "openai/gpt-6-astra", MOCK_AGENT_MODEL_DISPLAY = "GPT-6 Astra", MOCK_AGENT_CONFIG_SET_LOG = {route_log_value}, MOCK_AGENT_PROMPT_LOG = {prompt_log_value}, MOCK_AGENT_RESPONSE = "OpenCode replied: reply-copper-928", MOCK_AGENT_RESPONSE_MATCH_TEXT = "future-maple-440", MOCK_AGENT_RESPONSE_MATCH = "hidden-future-reply-990" }}
[agents.model_override]
env = "MOCK_AGENT_INJECTED_MODEL"

[[agents]]
name = "Codex fixture"
slug = "codex"
[agents.distribution.local]
command = {mock_agent}
env = {{ MOCK_AGENT_MODEL_NAME = "codex", MOCK_AGENT_VERSIONED_MODEL = "1", MOCK_AGENT_MODEL_VALUE = "gpt-6-astra", MOCK_AGENT_MODEL_DISPLAY = "GPT-6 Astra", MOCK_AGENT_CONFIG_SET_LOG = {route_log_value}, MOCK_AGENT_PROMPT_LOG = {prompt_log_value}, MOCK_AGENT_RESPONSE = "Codex replied: reply-silver-314", MOCK_AGENT_STARTUP_DELAY_MS_CODEX = "2500" }}
[agents.model_override]
env = "MOCK_AGENT_INJECTED_MODEL"

[[agents]]
name = "Disabled Claude"
slug = "claude-code"
[agents.distribution.local]
command = "/usr/bin/false"
"#
    );
    let refreshed_at: i64 = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .try_into()
        .unwrap();
    let catalog = serde_json::json!({
        "codex": {
            "models": [{"value": "gpt-6-astra", "name": "GPT-6 Astra"}],
            "settings": [],
            "refreshed_at": refreshed_at
        }
    });
    let config = SessionConfig::new()
        .with_agent("opencode".to_owned())
        .with_extra_config_toml(extra_config)
        .with_agent_models_json(catalog.to_string());
    let mut session = TuiSession::spawn_with_config(30, 100, config).unwrap();
    session.wait_for_text("›", TIMEOUT).unwrap();
    session
        .submit_input("request-northwind-671")
        .expect("submit first turn");
    session
        .wait_for_text("reply-copper-928", TIMEOUT)
        .expect("OpenCode reply");
    session
        .submit_input("request-future-maple-440")
        .expect("submit future turn");
    session
        .wait_for_text("hidden-future-reply-990", TIMEOUT)
        .expect("future reply before rewind");
    let journals_before_rewind = journal_paths(&session);

    // Focus the latest response, then move to A/replyA. Enter rolls back
    // through that response while preserving the earlier visible context.
    session.send_str("\x1b[27u").expect("enter normal mode");
    session
        .wait_for_text("NORMAL", TIMEOUT)
        .expect("normal mode");
    session.send_str("[").expect("focus latest response");
    session
        .wait_for_text("↶", TIMEOUT)
        .expect("response focus opened");
    session.send_str("[").expect("focus prior response");
    session
        .wait_for_text("Turn 1 of 2", TIMEOUT)
        .expect("prior response focused");
    session
        .send_key(tui_pty_e2e::Key::Enter)
        .expect("hide future turn");
    session
        .wait_for(
            |_| {
                journals_before_rewind.iter().any(|path| {
                    nori_conversation::ConversationLog::open(path).is_ok_and(|log| {
                        let rewind = (1..=i64::try_from(log.len()).unwrap_or(i64::MAX))
                            .rev()
                            .find(|id| log.kind(*id) == Some(nori_conversation::Kind::Rewind));
                        rewind.is_some_and(|rewind| {
                            log.visible_named(nori_conversation::Kind::Command, "backend-bound")
                                .iter()
                                .any(|id| *id > rewind)
                        })
                    })
                })
            },
            TIMEOUT,
        )
        .unwrap_or_else(|error| {
            panic!(
                "rewind and binding: {error}; records: {:?}",
                journals_before_rewind
                    .iter()
                    .map(|path| nori_conversation::ConversationLog::open(path)
                        .map(|log| log.all().filter_map(Result::ok).collect::<Vec<_>>()))
                    .collect::<Vec<_>>()
            )
        });
    session
        .wait_for_text("OpenCode fixture", TIMEOUT)
        .expect("rewound OpenCode session is ready");
    let branch_journals = journal_paths(&session);

    session
        .send_str("i")
        .expect("return composer to insert mode");
    session
        .wait_for_text("INSERT", TIMEOUT)
        .expect("insert mode");
    session.submit_input("/model").expect("open model picker");
    session
        .wait_for_text("Select model", TIMEOUT)
        .expect("model picker");
    session.type_input("GPT-6 Astra").expect("filter model");
    session
        .send_key(tui_pty_e2e::Key::Enter)
        .expect("select Codex model route");
    session
        .wait_for(|screen| !screen.contains("Select model"), TIMEOUT)
        .expect("model picker closed");
    session
        .type_input("request-birch-582")
        .expect("type draft while Codex starts");
    session
        .wait_for_text("Codex fixture", TIMEOUT)
        .expect("Codex provider status");
    session
        .wait_for(
            |_| {
                std::fs::read_to_string(&route_log)
                    .is_ok_and(|log| log.contains("codex\tgpt-6-astra"))
            },
            TIMEOUT,
        )
        .expect("Codex model applied");
    assert!(
        session.screen_contents().contains("request-birch-582"),
        "composer draft should survive the model handoff"
    );
    session
        .send_key(tui_pty_e2e::Key::Enter)
        .expect("submit preserved draft");
    session
        .wait_for_text("reply-silver-314", TIMEOUT)
        .expect("Codex reply");

    let prompts = std::fs::read_to_string(&prompt_log).expect("prompt route log");
    let prompts: Vec<_> = prompts
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect();
    assert_eq!(prompts.len(), 3, "{prompts:?}");
    assert_eq!(prompts[0]["agent"], "opencode");
    assert_eq!(prompts[1]["agent"], "opencode");
    assert_eq!(prompts[2]["agent"], "codex");
    let handed_off_context = prompts[2]["content"].to_string();
    assert_eq!(
        handed_off_context.matches("request-northwind-671").count(),
        1
    );
    assert_eq!(handed_off_context.matches("reply-copper-928").count(), 1);
    assert_eq!(handed_off_context.matches("request-birch-582").count(), 1);
    assert!(!handed_off_context.contains("future-maple-440"));
    assert!(!handed_off_context.contains("hidden-future-reply-990"));
    assert_eq!(journal_paths(&session), branch_journals);
    let screen = session.screen_contents();
    assert!(screen.contains("Codex fixture"), "{screen}");
    assert!(!screen.contains("option set:"), "{screen}");
    assert!(!screen.contains("option updated:"), "{screen}");
    let _ = std::fs::remove_file(route_log);
    let _ = std::fs::remove_file(prompt_log);
}
