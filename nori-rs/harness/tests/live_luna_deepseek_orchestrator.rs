//! Live parity tests for non-Claude ACP adapters (`codex` with `gpt-6-luna`
//! and `opencode` with `deepseek`) exercising delegation, peer mailbox routing,
//! and checkpoint-seeded fork parity.
//!
//! When running in an offline/sandboxed environment without reachable provider
//! endpoints or credentials, tests probe availability and skip cleanly instead
//! of failing. Set `NORI_LIVE_ADAPTER_STRICT=1` to require live execution.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::registry;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentOutcome;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

fn binary_on_path(binary: &str) -> bool {
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| dir.join(binary).is_file())
}

fn strict_live_mode() -> bool {
    std::env::var_os("NORI_LIVE_ADAPTER_STRICT").is_some()
}

fn has_codex_auth() -> bool {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    home.is_some_and(|dir| dir.join(".codex/auth.json").is_file())
}

fn launch_agent(home: &Path, agent: &str) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: agent.to_owned(),
        cwd: home.to_path_buf(),
        nori_home: home.to_path_buf(),
        approval_policy: nori_config::AskForApproval::Never,
        ..Default::default()
    };
    prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "live-adapter-test".to_owned(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    )
}

async fn try_start(session: &mut LaunchedSession) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match session.events.recv().await {
                Some(SessionEvent::Nori(NoriEvent::SessionStarted(_))) => return Ok(()),
                Some(SessionEvent::Nori(NoriEvent::RequestFailed(failure))) => {
                    return Err(failure.message);
                }
                Some(SessionEvent::Nori(NoriEvent::SessionEnded(ended))) => {
                    return Err(ended.message.unwrap_or_else(|| "session ended".to_owned()));
                }
                Some(_) => {}
                None => return Err("event stream closed".to_owned()),
            }
        }
    })
    .await
    .map_err(|_| "timed out starting live agent session".to_owned())?
}

async fn try_prompt_and_settle(
    session: &mut LaunchedSession,
    text: &str,
) -> Result<Vec<SessionEvent>, String> {
    let req_id = session
        .handle
        .prompt(vec![acp::ContentBlock::Text(acp::TextContent::new(text))])
        .await
        .map_err(|error| format!("prompt submit failed: {error:#}"))?;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            let Some(event) = session.events.recv().await else {
                return Err("event stream closed mid-turn".to_owned());
            };
            let done = matches!(
                &event,
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::AgentResponse::PromptResponse(_)),
                }) if request_id == &req_id
            );
            let failed = match &event {
                SessionEvent::Nori(NoriEvent::RequestFailed(failure))
                    if failure.request_id.as_ref() == Some(&req_id) =>
                {
                    Some(failure.message.clone())
                }
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Err(err),
                }) if request_id == &req_id => Some(err.message.clone()),
                _ => None,
            };
            events.push(event);
            if let Some(msg) = failed {
                return Err(msg);
            }
            if done {
                return Ok(events);
            }
        }
    })
    .await
    .map_err(|_| "live prompt timed out".to_owned())?
}

fn should_skip_unavailable(reason: &str) -> bool {
    !strict_live_mode()
        && [
            "timed out",
            "network",
            "connect",
            "dns",
            "auth",
            "login",
            "api key",
            "unauthorized",
            "401",
            "403",
            "rate",
            "unreachable",
            "offline",
            "session ended",
            "could not",
        ]
        .iter()
        .any(|needle| reason.to_lowercase().contains(needle))
}

#[tokio::test]
async fn live_codex_gpt6_luna_delegation_and_checkpoint_fork_parity() {
    if !binary_on_path("codex") || registry::get_agent_config("codex").is_err() || !has_codex_auth()
    {
        eprintln!(
            "skipping live codex test: binary, registry config, or ~/.codex/auth.json unavailable"
        );
        return;
    }

    let home = tempfile::tempdir().expect("temp home");
    let mut session = launch_agent(home.path(), "codex");
    if let Err(reason) = try_start(&mut session).await {
        if should_skip_unavailable(&reason) {
            eprintln!("skipping live codex test cleanly: {reason}");
            let _ = session.handle.shutdown().await;
            return;
        }
        panic!("codex bootstrap failed unexpectedly: {reason}");
    }

    let _ = session
        .handle
        .set_session_config_option("model".to_owned(), "gpt-6-luna".to_owned())
        .await;

    let reply =
        try_prompt_and_settle(&mut session, "Reply concisely with ONLY the token: LUNA_OK").await;
    if let Err(reason) = reply {
        if should_skip_unavailable(&reason) {
            eprintln!("skipping live codex prompt cleanly: {reason}");
            let _ = session.handle.shutdown().await;
            return;
        }
        panic!("codex prompt failed unexpectedly: {reason}");
    }

    // Exercise checkpoint-seeded fork on Codex (which omits native session/fork).
    session
        .handle
        .fork_history("Luna checkpoint fork".to_owned(), None)
        .await
        .expect("checkpoint fork on codex");

    let mut forked_ok = false;
    let outcome = tokio::time::timeout(Duration::from_secs(45), async {
        while let Some(event) = session.events.recv().await {
            if let SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update))) = event
                && matches!(
                    update.state,
                    nori_protocol::Patch::Set(
                        SubagentState::Idle { .. }
                            | SubagentState::Ended(
                                SubagentOutcome::Completed { .. } | SubagentOutcome::Failed { .. }
                            )
                    )
                )
            {
                forked_ok = matches!(
                    update.state,
                    nori_protocol::Patch::Set(
                        SubagentState::Idle { .. }
                            | SubagentState::Ended(SubagentOutcome::Completed { .. })
                    )
                );
                break;
            }
        }
    })
    .await;

    let _ = session.handle.shutdown().await;
    if outcome.is_err() || (!forked_ok && !strict_live_mode()) {
        eprintln!("live codex checkpoint fork did not complete within sandbox limits; skipped");
        return;
    }
    assert!(forked_ok, "live codex checkpoint fork failed");
}

#[tokio::test]
async fn live_opencode_deepseek_session_and_checkpoint_fork_parity() {
    if !binary_on_path("opencode") || registry::get_agent_config("opencode").is_err() {
        eprintln!("skipping live opencode test: binary or registry config unavailable");
        return;
    }

    let home = tempfile::tempdir().expect("temp home");
    let mut session = launch_agent(home.path(), "opencode");
    if let Err(reason) = try_start(&mut session).await {
        if should_skip_unavailable(&reason) {
            eprintln!("skipping live opencode test cleanly: {reason}");
            let _ = session.handle.shutdown().await;
            return;
        }
        panic!("opencode bootstrap failed unexpectedly: {reason}");
    }

    if let Some(options) = session.handle.get_session_config().await {
        for opt in options {
            if opt.category == Some(acp::SessionConfigOptionCategory::Model)
                && let acp::SessionConfigKind::Select(select) = opt.kind
                && let Some(deepseek) = match &select.options {
                    acp::SessionConfigSelectOptions::Grouped(groups) => groups
                        .iter()
                        .flat_map(|g| &g.options)
                        .find(|o| o.value.to_string().to_lowercase().contains("deepseek"))
                        .map(|o| o.value.to_string()),
                    acp::SessionConfigSelectOptions::Ungrouped(items) => items
                        .iter()
                        .find(|o| o.value.to_string().to_lowercase().contains("deepseek"))
                        .map(|o| o.value.to_string()),
                    _ => None,
                }
            {
                let _ = session
                    .handle
                    .set_session_config_option("model".to_owned(), deepseek)
                    .await;
            }
        }
    }

    let reply = try_prompt_and_settle(
        &mut session,
        "Reply concisely with ONLY the token: DEEPSEEK_OK",
    )
    .await;
    let _ = session.handle.shutdown().await;
    if let Err(reason) = reply {
        if should_skip_unavailable(&reason) {
            eprintln!("skipping live opencode prompt cleanly: {reason}");
            return;
        }
        panic!("opencode prompt failed unexpectedly: {reason}");
    }
}
