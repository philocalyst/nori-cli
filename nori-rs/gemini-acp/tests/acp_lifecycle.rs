#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

async fn read_for_id(
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    id: u64,
) -> Value {
    loop {
        let mut line = String::new();
        assert_ne!(
            reader.read_line(&mut line).await.unwrap(),
            0,
            "ACP server closed early"
        );
        let message: Value = serde_json::from_str(&line).unwrap();
        if message.get("id") == Some(&json!(id)) {
            return message;
        }
    }
}

async fn send(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, value: Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn run_lifecycle(auth: &str, vertex: bool) {
    let node = std::env::var_os("NORI_GEMINI_TEST_NODE")
        .expect("set NORI_GEMINI_TEST_NODE to a Node executable");
    let cwd = std::env::temp_dir().join(format!("nori-gemini-acp-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&cwd).unwrap();
    let (client_pipe, server_pipe) = tokio::io::duplex(64 * 1024);
    let server = tokio::spawn(nori_gemini_acp::serve_over_pipe(
        server_pipe,
        PathBuf::from(node),
        fixture("fake_core.mjs"),
        HashMap::from([
            (OsString::from("NORI_GEMINI_AUTH"), OsString::from(auth)),
            (
                OsString::from("GEMINI_API_KEY"),
                OsString::from("offline-fake-api-key"),
            ),
            (
                OsString::from("NORI_GEMINI_EXPECT_AUTH"),
                OsString::from(auth),
            ),
            (
                OsString::from("GOOGLE_GENAI_USE_VERTEXAI"),
                OsString::from(vertex.to_string()),
            ),
        ]),
    ));
    let (read, mut write) = tokio::io::split(client_pipe);
    let mut read = BufReader::new(read);

    send(&mut write, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","clientInfo":{"name":"offline-test","version":"1"},"capabilities":{}}})).await;
    let initialized = read_for_id(&mut read, 1).await;
    assert!(initialized.get("result").is_some(), "{initialized}");
    assert_eq!(
        initialized["result"]["agentCapabilities"]["promptCapabilities"]["image"],
        true
    );

    send(&mut write, json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":cwd,"mcpServers":[],"additionalDirectories":[]}})).await;
    let created = read_for_id(&mut read, 2).await;
    assert!(created.get("result").is_some(), "{created}");
    let session_id = created["result"]["sessionId"].as_str().unwrap().to_owned();
    assert_eq!(
        created["result"]["configOptions"][0]["currentValue"],
        "gemini-2.5-pro"
    );

    send(&mut write, json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}})).await;
    let prompted = read_for_id(&mut read, 3).await;
    assert_eq!(prompted["result"]["stopReason"], "end_turn", "{prompted}");

    // An adjacent cancel must latch even if the engine has not yet started the prompt.
    for request_id in 10..15 {
        let prompt = json!({"jsonrpc":"2.0","id":request_id,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"cancel-case"}]}});
        let cancel =
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session_id}});
        write
            .write_all(format!("{prompt}\n{cancel}\n").as_bytes())
            .await
            .unwrap();
        let cancelled = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_for_id(&mut read, request_id),
        )
        .await
        .expect("immediate cancellation timed out");
        assert_eq!(
            cancelled["result"]["stopReason"], "cancelled",
            "{cancelled}"
        );
    }
    send(&mut write, json!({"jsonrpc":"2.0","id":16,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}})).await;
    let next = read_for_id(&mut read, 16).await;
    assert_eq!(next["result"]["stopReason"], "end_turn", "{next}");

    send(&mut write, json!({"jsonrpc":"2.0","id":18,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"cancel-case"}]}})).await;
    loop {
        let mut line = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), read.read_line(&mut line))
            .await
            .expect("started prompt timed out")
            .unwrap();
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["params"]["update"]["content"]["text"] == "cancel fixture started" {
            break;
        }
    }
    send(
        &mut write,
        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session_id}}),
    )
    .await;
    let cancelled = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_for_id(&mut read, 18),
    )
    .await
    .expect("active cancellation timed out");
    assert_eq!(
        cancelled["result"]["stopReason"], "cancelled",
        "{cancelled}"
    );

    send(&mut write, json!({"jsonrpc":"2.0","id":17,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"tool-case"}]}})).await;
    let mut tool_result = None;
    loop {
        let mut line = String::new();
        tokio::time::timeout(std::time::Duration::from_secs(5), read.read_line(&mut line))
            .await
            .expect("tool result timed out")
            .unwrap();
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "tool_call_update"
            && message["params"]["update"]["status"] == "completed"
        {
            tool_result = Some(message["params"]["update"].clone());
        }
        if message["id"] == 17 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            break;
        }
    }
    let tool_result = tool_result.expect("tool result update");
    assert_eq!(tool_result["rawOutput"]["resultDisplay"], "human display");
    assert_eq!(
        tool_result["rawOutput"]["data"]["structuredContent"],
        json!({"count":7})
    );
    assert_eq!(
        tool_result["rawOutput"]["responseParts"][1]["inlineData"]["data"],
        "aGVsbG8="
    );
    assert_eq!(tool_result["content"][0]["content"]["text"], "raw text");
    assert_eq!(
        tool_result["content"][1]["content"]["text"],
        "model-visible text"
    );
    assert_eq!(tool_result["content"][2]["content"]["type"], "image");

    send(
        &mut write,
        json!({"jsonrpc":"2.0","id":4,"method":"session/close","params":{"sessionId":session_id}}),
    )
    .await;
    assert!(read_for_id(&mut read, 4).await.get("result").is_some());
    drop(write);
    drop(read);
    server.await.unwrap().unwrap();
    std::fs::remove_dir_all(cwd).unwrap();
}

// This is an end-to-end ACP wire test. The Core fixture is local and fake,
// so no Gemini credentials or model API are used.
#[tokio::test]
#[ignore = "requires Node; set NORI_GEMINI_TEST_NODE to its executable path"]
async fn acp_lifecycle_uses_gemini_api_key_auth() {
    run_lifecycle("gemini", false).await;
}

#[tokio::test]
#[ignore = "requires Node; set NORI_GEMINI_TEST_NODE to its executable path"]
async fn acp_lifecycle_uses_vertex_auth_when_selected() {
    run_lifecycle("vertex", true).await;
}
