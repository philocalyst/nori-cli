//! Behavior tests for native `/compact` forwarding and branch-at-head `/fork`.
//!
//! These drive a real harness session against the mock ACP agent and observe
//! only external behavior: emitted `SessionEvent`s and the recorded ACP wire
//! log (session ids and prompt content that actually reached the agent).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::LaunchedSession;
use nori_harness::runtime::SessionLaunchSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::launch_session;
use nori_harness::runtime::prepare_agent;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp;
use pretty_assertions::assert_eq;
use serial_test::serial;

struct EnvGuard(&'static str);

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: tests that mutate the mock-agent environment run serially.
        unsafe { std::env::remove_var(self.0) };
    }
}

/// Launch a mock-agent session with ACP wire recording enabled so tests can
/// observe the exact prompts and session ids that reached the agent.
#[expect(
    clippy::expect_used,
    reason = "focused failure for an agent preparation failure"
)]
async fn launch_with_wire_log(cwd: &Path, wire_log_dir: &Path) -> LaunchedSession {
    let config = NoriConfig {
        active_agent: "mock-model".to_string(),
        cwd: cwd.to_path_buf(),
        nori_home: cwd.to_path_buf(),
        acp_proxy: nori_config::AcpProxyConfig {
            enabled: true,
            log_dir: wire_log_dir.to_path_buf(),
        },
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        handoff: None,
        config: Arc::new(config),
        cli_version: "native-compact-branch-test".to_string(),
        session_context: None,
        initial_context: None,
    })
    .await
    .expect("prepare mock agent");
    launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::New,
    })
}

#[expect(
    clippy::expect_used,
    reason = "focused failures for a missing bootstrap"
)]
async fn wait_for_started(session: &mut LaunchedSession) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match session.events.recv().await.expect("event stream closed") {
                SessionEvent::Nori(NoriEvent::SessionStarted(started)) => {
                    return started.acp_session_id.to_string();
                }
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    panic!("session bootstrap failed: {failure:?}");
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("session should start")
}

/// Drive the event stream until one prompt turn completes (a `PromptResponse`
/// arrives for the given request id), returning whether a `ContextCompacted`
/// event was observed along the way.
#[expect(clippy::expect_used, reason = "focused failures for a stalled prompt")]
async fn drive_prompt_to_completion(
    session: &mut LaunchedSession,
    request_id: &acp::v1::RequestId,
) -> bool {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut saw_context_compacted = false;
        loop {
            match session.events.recv().await.expect("event stream closed") {
                SessionEvent::Nori(NoriEvent::ContextCompacted(_)) => {
                    saw_context_compacted = true;
                }
                SessionEvent::Acp(AcpEvent::Response {
                    request_id: response_id,
                    response: Ok(acp::v1::AgentResponse::PromptResponse(_)),
                }) if &response_id == request_id => return saw_context_compacted,
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    panic!("prompt failed: {failure:?}");
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("prompt should complete")
}

/// The recorded `session/prompt` params that reached the agent, in order.
#[expect(
    clippy::expect_used,
    reason = "focused failures for a missing wire log"
)]
fn recorded_prompts(wire_log_dir: &Path) -> Vec<serde_json::Value> {
    let mut wire_logs = std::fs::read_dir(wire_log_dir)
        .expect("wire log directory")
        .map(|entry| entry.expect("wire log entry").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "jsonl")
        })
        .collect::<Vec<_>>();
    assert_eq!(
        wire_logs.len(),
        1,
        "one ACP child should produce one wire log"
    );
    let wire_log = wire_logs.pop().expect("ACP wire log");
    std::fs::read_to_string(wire_log)
        .expect("read ACP wire log")
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("valid wire log line"))
        .filter(|record| {
            record["direction"] == "client_to_agent"
                && record["message"]["method"] == "session/prompt"
        })
        .map(|record| record["message"]["params"].clone())
        .collect()
}

fn first_prompt_text(params: &serde_json::Value) -> &str {
    params["prompt"][0]["text"].as_str().unwrap_or_default()
}

fn prompt_session_id(params: &serde_json::Value) -> &str {
    params["sessionId"].as_str().unwrap_or_default()
}

async fn submit_text(session: &LaunchedSession, text: &str) -> acp::v1::RequestId {
    session
        .handle
        .prompt(vec![acp::v1::ContentBlock::Text(
            acp::v1::TextContent::new(text),
        )])
        .await
        .unwrap_or_else(|error| panic!("submit prompt {text:?}: {error}"))
}

#[expect(clippy::expect_used, reason = "focused failure for a missing tempdir")]
fn temp_dirs() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("create session directory");
    let wire = temp.path().join("acp-wire");
    (temp, wire)
}

#[tokio::test]
#[serial]
async fn rewind_rebinds_to_fresh_engine_with_only_the_visible_prefix() {
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;
    let original_engine = wait_for_started(&mut session).await;
    session
        .handle
        .set_session_config_option("model".to_string(), "mock-model-fast".to_string())
        .await
        .expect("select model");
    let first = submit_text(&session, "Keep this request").await;
    let (canonical_id, head) = tokio::time::timeout(Duration::from_secs(10), async {
        let mut boundary = None;
        let mut completed = false;
        loop {
            match session.events.recv().await.expect("event stream") {
                SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(value)) => {
                    boundary = Some((value.transcript_id, value.end));
                }
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::v1::AgentResponse::PromptResponse(_)),
                }) if request_id == first => completed = true,
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                    nori_protocol::SessionPhase::Idle,
                )) if completed && boundary.is_some() => break boundary.unwrap(),
                _ => {}
            }
        }
    })
    .await
    .expect("first turn completes");

    let discarded = submit_text(&session, "Discard this secret").await;
    drive_prompt_to_completion(&mut session, &discarded).await;
    // Wait until the reducer is idle before rewinding.
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            session.events.recv().await,
            Some(SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                nori_protocol::SessionPhase::Idle
            )))
        ) {}
    })
    .await
    .expect("second turn becomes idle");
    session
        .handle
        .rewind_and_rebind(canonical_id.clone(), Some(head))
        .await
        .expect("rewind and rebind");
    let options = session
        .handle
        .get_session_config()
        .await
        .expect("session options");
    assert!(options.iter().any(|option| matches!(&option.kind,
        acp::v1::SessionConfigKind::Select(select)
            if option.category == Some(acp::v1::SessionConfigOptionCategory::Model)
                && select.current_value.to_string() == "mock-model-fast"
    )));
    let next = submit_text(&session, "Continue after rewind").await;
    drive_prompt_to_completion(&mut session, &next).await;
    let recorder = session
        .handle
        .conversation_recorder()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorder.session_id(), canonical_id);
    let prompts = recorded_prompts(&wire);
    assert_eq!(prompts.len(), 3);
    assert_ne!(prompt_session_id(&prompts[2]), original_engine);
    let retry = serde_json::to_string(&prompts[2]).unwrap();
    assert!(retry.contains("Keep this request"), "{retry}");
    assert!(!retry.contains("Discard this secret"), "{retry}");
    assert!(retry.contains("Continue after rewind"), "{retry}");
}

#[tokio::test]
#[serial]
async fn native_compact_forwards_without_swapping_session() {
    // SAFETY: mock-agent env-mutating tests run serially.
    unsafe { std::env::set_var("MOCK_AGENT_ADVERTISE_COMPACT", "1") };
    let _guard = EnvGuard("MOCK_AGENT_ADVERTISE_COMPACT");
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;

    let bootstrap_session_id = wait_for_started(&mut session).await;

    // Warm-up turn flushes the agent's AvailableCommandsUpdate into the runtime
    // so the subsequent /compact is recognized as a native command.
    let warmup = submit_text(&session, "warmup").await;
    drive_prompt_to_completion(&mut session, &warmup).await;

    session.handle.compact().await.expect("compact");
    // The native compact is forwarded as an ordinary turn ("/compact").
    let compact_saw_context_compacted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::ContextCompacted(_)) =
                session.events.recv().await.expect("event stream closed")
            {
                return true;
            }
        }
    })
    .await
    .expect("native compact should emit ContextCompacted");
    assert!(compact_saw_context_compacted);

    let follow_up = submit_text(&session, "hi").await;
    drive_prompt_to_completion(&mut session, &follow_up).await;

    session.handle.shutdown().await.expect("shutdown");

    let prompts = recorded_prompts(&wire);
    assert_eq!(
        prompts.len(),
        3,
        "warm-up, native /compact, and follow-up prompts should all reach the agent"
    );
    // All three prompts stayed on the original session: native compact did not
    // swap the ACP session id.
    for params in &prompts {
        assert_eq!(prompt_session_id(params), bootstrap_session_id);
    }
    assert_eq!(first_prompt_text(&prompts[1]), "/compact");
    // The follow-up prompt is verbatim: no summary prefix, no summarize-and-swap.
    assert_eq!(first_prompt_text(&prompts[2]), "hi");
}

/// Run `/compact` on a session that has had one prompt, and return the prompts the agent got and
/// whether the compaction completed.
#[expect(
    clippy::expect_used,
    reason = "test helpers fail with a focused boundary diagnostic"
)]
async fn compact_after_one_prompt(
    summary: &str,
    nudged_summary: &str,
) -> (Vec<serde_json::Value>, String, bool) {
    // SAFETY: mock-agent env-mutating tests run serially.
    unsafe {
        std::env::set_var("MOCK_AGENT_SUMMARY_RESPONSE", summary);
        std::env::set_var("MOCK_AGENT_NUDGED_SUMMARY_RESPONSE", nudged_summary);
    }
    let _summary = EnvGuard("MOCK_AGENT_SUMMARY_RESPONSE");
    let _nudged = EnvGuard("MOCK_AGENT_NUDGED_SUMMARY_RESPONSE");
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;
    let bootstrap_session_id = wait_for_started(&mut session).await;
    let warm_up = submit_text(&session, "warm up").await;
    drive_prompt_to_completion(&mut session, &warm_up).await;

    session.handle.compact().await.expect("compact");
    let compacted = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match session.events.recv().await.expect("event stream closed") {
                SessionEvent::Nori(NoriEvent::ContextCompacted(_)) => return true,
                SessionEvent::Nori(NoriEvent::Notice(notice))
                    if notice.message.starts_with("Compaction failed") =>
                {
                    return false;
                }
                _ => {}
            }
        }
    })
    .await
    .expect("compaction should end");

    let follow_up = submit_text(&session, "hi").await;
    drive_prompt_to_completion(&mut session, &follow_up).await;
    session.handle.shutdown().await.expect("shutdown");
    (recorded_prompts(&wire), bootstrap_session_id, compacted)
}

#[tokio::test]
#[serial]
async fn compact_without_native_support_swaps_and_injects_summary() {
    let (prompts, bootstrap_session_id, compacted) =
        compact_after_one_prompt("Here is what happened.", "## Objective\n- Say hi").await;

    assert!(compacted);
    assert_eq!(
        prompts.len(),
        4,
        "warm-up, summary, reminder, and follow-up prompts should all reach the agent"
    );
    // The summary was asked for on the original session, and asked once more when it skipped the
    // template; the follow-up ran on a freshly created session (summarize-and-swap).
    assert!(
        first_prompt_text(&prompts[1]).starts_with("You MUST summarize the conversation above")
    );
    assert_eq!(first_prompt_text(&prompts[2]), nori_harness::compact::NUDGE);
    for params in &prompts[..3] {
        assert_eq!(prompt_session_id(params), bootstrap_session_id);
    }
    assert_ne!(
        prompt_session_id(&prompts[3]),
        bootstrap_session_id,
        "fallback compact should create and swap to a new session"
    );
    let follow_up = first_prompt_text(&prompts[3]);
    assert!(
        follow_up.starts_with("<conversation-checkpoint>\n"),
        "the next prompt opens with the checkpoint: {follow_up}"
    );
    assert!(
        follow_up.contains("<summary>\n## Objective\n- Say hi\n</summary>"),
        "{follow_up}"
    );
    // The newest exchanges come along verbatim, the compaction itself not.
    assert!(
        follow_up.contains("<recent-context>\nUser: warm up\n"),
        "{follow_up}"
    );
    assert!(!follow_up.contains("You MUST summarize"), "{follow_up}");
    // The checkpoint is a block of its own ahead of the user's prompt.
    assert!(
        follow_up.trim_end().ends_with("</conversation-checkpoint>"),
        "{follow_up}"
    );
    assert_eq!(
        prompts[3]["prompt"][0]["annotations"]["audience"],
        serde_json::json!(["assistant"])
    );
    assert_eq!(prompts[3]["prompt"][1]["text"].as_str(), Some("hi"));
}

#[tokio::test]
#[serial]
async fn a_summary_that_never_fills_the_template_keeps_the_session() {
    let (prompts, bootstrap_session_id, compacted) =
        compact_after_one_prompt("Here is what happened.", "Still prose.").await;

    assert!(!compacted);
    assert_eq!(prompts.len(), 4);
    for params in &prompts {
        assert_eq!(prompt_session_id(params), bootstrap_session_id);
    }
    assert_eq!(first_prompt_text(&prompts[3]), "hi");
}

#[tokio::test]
#[serial]
async fn branch_forks_active_session() {
    // SAFETY: mock-agent env-mutating tests run serially.
    unsafe { std::env::set_var("MOCK_AGENT_SUPPORT_SESSION_FORK", "1") };
    let _guard = EnvGuard("MOCK_AGENT_SUPPORT_SESSION_FORK");
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;

    let bootstrap_session_id = wait_for_started(&mut session).await;

    session
        .handle
        .branch()
        .await
        .expect("branch should succeed");

    // The fork response carries the new (forked) session id.
    let forked_session_id = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEvent::Acp(AcpEvent::Response {
                response: Ok(acp::v1::AgentResponse::ForkSessionResponse(response)),
                ..
            }) = session.events.recv().await.expect("event stream closed")
            {
                return response.session_id.to_string();
            }
        }
    })
    .await
    .expect("branch should fork via session/fork");
    assert_ne!(forked_session_id, bootstrap_session_id);

    // The active session is now the forked one: the next prompt targets it.
    let after_branch = submit_text(&session, "after branch").await;
    drive_prompt_to_completion(&mut session, &after_branch).await;

    session.handle.shutdown().await.expect("shutdown");

    let prompts = recorded_prompts(&wire);
    assert_eq!(prompts.len(), 1, "one prompt should have reached the agent");
    assert_eq!(
        prompt_session_id(&prompts[0]),
        forked_session_id,
        "post-branch prompt should target the forked session"
    );
}

#[tokio::test]
#[serial]
async fn branch_after_rewind_seeds_only_the_visible_history() {
    if std::env::var_os("NORI_BRANCH_REWIND_TEST_INNER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "branch_after_rewind_seeds_only_the_visible_history",
                "--nocapture",
            ])
            .env("NORI_BRANCH_REWIND_TEST_INNER", "1")
            .env("MOCK_AGENT_SUPPORT_SESSION_FORK", "1")
            .status()
            .expect("run isolated branch fixture");
        assert!(status.success(), "isolated branch test failed");
        return;
    }
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;
    wait_for_started(&mut session).await;
    let first = submit_text(&session, "kept parent turn").await;
    let boundary = tokio::time::timeout(Duration::from_secs(10), async {
        let mut boundary = None;
        let mut completed = false;
        loop {
            match session.events.recv().await.expect("event stream") {
                SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(value)) => {
                    boundary = Some(value)
                }
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::v1::AgentResponse::PromptResponse(_)),
                }) if request_id == first => completed = true,
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                    nori_protocol::SessionPhase::Idle,
                )) if completed && boundary.is_some() => {
                    break boundary.expect("first response boundary");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("first turn completes");
    let second = submit_text(&session, "discarded parent turn").await;
    drive_prompt_to_completion(&mut session, &second).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !matches!(
            session.events.recv().await,
            Some(SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                nori_protocol::SessionPhase::Idle
            )))
        ) {}
    })
    .await
    .expect("second turn becomes idle");
    let parent = session
        .handle
        .conversation_recorder()
        .await
        .expect("recorder request")
        .expect("parent journal");
    parent.flush().await.expect("flush parent");
    assert!(
        std::fs::read_to_string(parent.transcript_path())
            .expect("legacy transcript")
            .contains("discarded parent turn")
    );
    session
        .handle
        .rewind_and_rebind(boundary.transcript_id.clone(), Some(boundary.end))
        .await
        .expect("rewind parent");
    session
        .handle
        .branch()
        .await
        .expect("branch rewound session");
    let forked = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::SessionForked(forked)) =
                session.events.recv().await.expect("event stream")
            {
                break forked;
            }
        }
    })
    .await
    .expect("fork event");
    assert_eq!(forked.previous_conversation_id, boundary.transcript_id);
    let loader = nori_harness::TranscriptLoader::new(temp.path().to_path_buf());
    let transcript = loader
        .load_transcript(parent.project_id(), &forked.new_conversation_id)
        .await
        .expect("load branch transcript");
    assert_eq!(
        transcript.meta.forked_from.as_deref(),
        Some(boundary.transcript_id.as_str())
    );
    let replay = nori_harness::backend::transcript_to_replay_session_events(&transcript);
    let replay = serde_json::to_string(&replay).expect("serialize branch replay");
    assert!(replay.contains("kept parent turn"), "{replay}");
    assert!(!replay.contains("discarded parent turn"), "{replay}");
    let next = submit_text(&session, "branch follow-up").await;
    drive_prompt_to_completion(&mut session, &next).await;
    let prompts = recorded_prompts(&wire);
    let last =
        serde_json::to_string(prompts.last().expect("branch prompt")).expect("serialize prompt");
    assert!(last.contains("kept parent turn"), "{last}");
    assert!(!last.contains("discarded parent turn"), "{last}");
}

#[tokio::test]
#[serial]
async fn interrupted_rewind_never_resumes_the_old_provider_session() {
    if std::env::var_os("NORI_REWIND_RECOVERY_TEST_INNER").is_none() {
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "interrupted_rewind_never_resumes_the_old_provider_session",
                "--nocapture",
            ])
            .env("NORI_REWIND_RECOVERY_TEST_INNER", "1")
            .env("MOCK_AGENT_SUPPORT_LOAD_SESSION", "1")
            .env("MOCK_AGENT_SUPPORT_SESSION_RESUME", "1")
            .status()
            .expect("run isolated recovery fixture");
        assert!(status.success(), "isolated recovery test failed");
        return;
    }
    let (temp, wire) = temp_dirs();
    let mut original = launch_with_wire_log(temp.path(), &wire).await;
    let old_engine = wait_for_started(&mut original).await;
    let first = submit_text(&original, "recovery keeps A").await;
    let boundary = tokio::time::timeout(Duration::from_secs(10), async {
        let mut boundary = None;
        let mut completed = false;
        loop {
            match original.events.recv().await.expect("event stream") {
                SessionEvent::Nori(NoriEvent::ConversationResponseBoundary(value)) => {
                    boundary = Some(value)
                }
                SessionEvent::Acp(AcpEvent::Response {
                    request_id,
                    response: Ok(acp::v1::AgentResponse::PromptResponse(_)),
                }) if request_id == first => completed = true,
                SessionEvent::Nori(NoriEvent::SessionPhaseChanged(
                    nori_protocol::SessionPhase::Idle,
                )) if completed && boundary.is_some() => {
                    break boundary.expect("response boundary");
                }
                _ => {}
            }
        }
    })
    .await
    .expect("first turn completes");
    let second = submit_text(&original, "recovery discards B").await;
    drive_prompt_to_completion(&mut original, &second).await;
    let recorder = original
        .handle
        .conversation_recorder()
        .await
        .expect("recorder request")
        .expect("journal");
    recorder.flush().await.expect("flush journal");
    original.handle.shutdown().await.expect("shutdown original");
    // Simulate a crash after the durable rewind record and before backend-bound is appended.
    let mut log =
        nori_conversation::ConversationLog::open(recorder.transcript_path().with_extension("pkl"))
            .expect("open journal");
    log.append(nori_conversation::Event::Rewind {
        head: Some(boundary.end),
    })
    .expect("commit interrupted rewind");
    let loader = nori_harness::TranscriptLoader::new(temp.path().to_path_buf());
    let transcript = loader
        .load_transcript(recorder.project_id(), recorder.session_id())
        .await
        .expect("load interrupted transcript");
    assert_eq!(transcript.latest_acp_session_id(), None);
    let resume_wire = temp.path().join("resume-wire");
    let config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: temp.path().to_path_buf(),
        nori_home: temp.path().to_path_buf(),
        acp_proxy: nori_config::AcpProxyConfig {
            enabled: true,
            log_dir: resume_wire.clone(),
        },
        ..Default::default()
    };
    let agent = prepare_agent(AgentPrepareSpec {
        handoff: None,
        config: Arc::new(config),
        cli_version: "rewind-recovery-test".to_owned(),
        session_context: None,
        initial_context: None,
    })
    .await
    .expect("prepare replacement");
    let mut resumed = launch_session(SessionLaunchSpec {
        agent,
        start: SessionStart::Resume(nori_harness::runtime::SessionResume {
            // Even a caller that cached the old id must not reactivate it.
            acp_session_id: Some(old_engine.clone()),
            transcript: Some(transcript),
        }),
    });
    wait_for_started(&mut resumed).await;
    let request = submit_text(&resumed, "continue recovery").await;
    drive_prompt_to_completion(&mut resumed, &request).await;
    let prompt = serde_json::to_string(
        recorded_prompts(&resume_wire)
            .last()
            .expect("recovery prompt"),
    )
    .expect("serialize prompt");
    assert!(prompt.contains("recovery keeps A"), "{prompt}");
    assert!(!prompt.contains("recovery discards B"), "{prompt}");
    let wire_text = std::fs::read_dir(&resume_wire)
        .expect("wire logs")
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .collect::<String>();
    assert!(!wire_text.contains("session/load"), "{wire_text}");
    assert!(!wire_text.contains("session/resume"), "{wire_text}");
}

#[tokio::test]
#[serial]
async fn branch_unsupported_agent_errors() {
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;

    let bootstrap_session_id = wait_for_started(&mut session).await;

    let error = session
        .handle
        .branch()
        .await
        .expect_err("branch should fail without fork support");
    assert!(
        error.to_string().contains("does not support branching"),
        "unexpected branch error: {error}"
    );

    // The session id is unchanged: a subsequent prompt still targets it.
    let after = submit_text(&session, "still here").await;
    drive_prompt_to_completion(&mut session, &after).await;

    session.handle.shutdown().await.expect("shutdown");

    let prompts = recorded_prompts(&wire);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompt_session_id(&prompts[0]), bootstrap_session_id);
}

#[tokio::test]
#[serial]
async fn branch_leaves_session_intact_when_fork_rpc_fails() {
    // SAFETY: mock-agent env-mutating tests run serially.
    unsafe {
        std::env::set_var("MOCK_AGENT_SUPPORT_SESSION_FORK", "1");
        std::env::set_var("MOCK_AGENT_FORK_SESSION_FAIL", "1");
    }
    let _support_guard = EnvGuard("MOCK_AGENT_SUPPORT_SESSION_FORK");
    let _fail_guard = EnvGuard("MOCK_AGENT_FORK_SESSION_FAIL");
    let (temp, wire) = temp_dirs();
    let mut session = launch_with_wire_log(temp.path(), &wire).await;

    let bootstrap_session_id = wait_for_started(&mut session).await;

    // The agent advertises fork, so the capability gate passes, but the fork
    // RPC itself fails. Branch should surface the error and leave the active
    // session untouched.
    session
        .handle
        .branch()
        .await
        .expect_err("branch should fail when the fork RPC errors");

    let after = submit_text(&session, "still here").await;
    drive_prompt_to_completion(&mut session, &after).await;

    session.handle.shutdown().await.expect("shutdown");

    let prompts = recorded_prompts(&wire);
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompt_session_id(&prompts[0]), bootstrap_session_id);
}
