use std::sync::Arc;
use std::time::Duration;

use nori_config::NoriConfig;
use nori_harness::runtime::AgentPrepareSpec;
use nori_harness::runtime::SessionStart;
use nori_harness::runtime::prepare_and_launch_session;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp;
use pretty_assertions::assert_eq;

fn content(text: &str) -> Vec<acp::v1::ContentBlock> {
    vec![acp::v1::ContentBlock::Text(acp::v1::TextContent::new(text))]
}

#[tokio::test]
async fn queued_steering_and_promotion_reach_the_agent_once_in_order() {
    if std::env::var_os("NORI_QUEUE_TEST_INNER").is_none() {
        let dir = tempfile::tempdir().expect("fixture directory");
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "queued_steering_and_promotion_reach_the_agent_once_in_order",
                "--nocapture",
            ])
            .env("NORI_QUEUE_TEST_INNER", "1")
            .env("MOCK_AGENT_DELAY_MS", "1200")
            .env("MOCK_AGENT_PROMPT_LOG", dir.path().join("prompts.jsonl"))
            .status()
            .expect("run isolated fixture");
        assert!(status.success());
        return;
    }
    let home = tempfile::tempdir().expect("session directory");
    let config = NoriConfig {
        active_agent: "mock-model".to_owned(),
        cwd: home.path().to_path_buf(),
        nori_home: home.path().to_path_buf(),
        ..Default::default()
    };
    let mut session = prepare_and_launch_session(
        AgentPrepareSpec {
            handoff: None,
            config: Arc::new(config),
            cli_version: "queue-test".to_owned(),
            session_context: None,
            initial_context: None,
        },
        SessionStart::New,
    );
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            match session.events.recv().await.expect("event stream") {
                SessionEvent::Nori(NoriEvent::SessionStarted(_)) => break,
                SessionEvent::Nori(NoriEvent::RequestFailed(failure)) => {
                    panic!("startup: {failure:?}")
                }
                SessionEvent::Acp(_) | SessionEvent::Nori(_) => {}
            }
        }
    })
    .await
    .expect("session startup");
    session
        .handle
        .prompt(content("active"))
        .await
        .expect("start idle prompt");
    let handle = session.handle.clone();
    let queued = tokio::spawn(async move { handle.prompt(content("queued")).await });
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SessionEvent::Nori(NoriEvent::QueueChanged(queue)) =
                session.events.recv().await.expect("event stream")
                && queue.prompts.iter().any(|p| p.contains("queued"))
            {
                break;
            }
        }
    })
    .await
    .expect("queued prompt visible");
    let handle = session.handle.clone();
    let steered = tokio::spawn(async move {
        let mut blocks = content("steer");
        blocks.push(acp::v1::ContentBlock::Image(acp::v1::ImageContent::new(
            "aW1hZ2U=",
            "image/png",
        )));
        blocks.push(acp::v1::ContentBlock::ResourceLink(
            acp::v1::ResourceLink::new("notes", "file:///tmp/notes.md"),
        ));
        handle.steer_content(blocks).await
    });
    tokio::time::timeout(Duration::from_secs(15), steered)
        .await
        .expect("steer starts")
        .expect("steer task")
        .expect("steer request");
    session
        .handle
        .promote_next_queued()
        .await
        .expect("promote queue");
    tokio::time::timeout(Duration::from_secs(15), queued)
        .await
        .expect("queued starts")
        .expect("queued task")
        .expect("queued request");
    let log = std::env::var_os("MOCK_AGENT_PROMPT_LOG").expect("prompt log");
    let entries: Vec<serde_json::Value> = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let data = std::fs::read_to_string(&log).unwrap_or_default();
            let entries: Vec<serde_json::Value> = data
                .lines()
                .map(|line| serde_json::from_str(line).expect("JSON prompt"))
                .collect();
            if entries.len() >= 3 {
                break entries;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all prompts reach the agent");
    let texts: Vec<&str> = entries
        .iter()
        .map(|entry| entry["text"].as_str().expect("text"))
        .collect();
    assert_eq!(texts, vec!["active", "steer", "queued"]);
    let blocks = entries[1]["content"].as_array().expect("content blocks");
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[1]["data"], "aW1hZ2U=");
    assert_eq!(blocks[2]["uri"], "file:///tmp/notes.md");
    session.handle.shutdown().await.expect("shutdown");
}
