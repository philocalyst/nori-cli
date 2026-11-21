//! OpenCode in Nori's process, end to end over ACP: an account signs in with an API key, and a
//! later connection of that account reads the gateway's catalog and answers through the
//! gateway, both played by a local server. No OpenCode program runs, nothing of OpenCode's own is
//! read, and nothing leaves the machine.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use nori_acp_kit::Host;
use nori_acp_kit::InProcessSpec;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::header_exists;
use wiremock::matchers::method;
use wiremock::matchers::path;

/// models.dev's catalog: the gateway, with one Anthropic-wire and one OpenAI-wire model, at the
/// fake server; and another provider, which OpenCode never offers.
fn models_dev(gateway: &str) -> Value {
    json!({
        "opencode": {
            "id": "opencode", "name": "OpenCode Zen", "npm": "@ai-sdk/openai-compatible", "api": gateway,
            "models": {
                "claude-test": {"id": "claude-test", "name": "Claude Test", "reasoning": true,
                    "tool_call": true, "release_date": "2026-10-01", "limit": {"context": 200000, "output": 64000},
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "provider": {"npm": "@ai-sdk/anthropic"}},
                "gpt-test": {"id": "gpt-test", "name": "GPT Test", "reasoning": true, "tool_call": true,
                    "release_date": "2026-09-01", "limit": {"context": 400000, "output": 128000},
                    "provider": {"npm": "@ai-sdk/openai"}}
            }
        },
        "anthropic": {
            "id": "anthropic", "name": "Anthropic", "env": ["ANTHROPIC_API_KEY"], "npm": "@ai-sdk/anthropic",
            "models": {"claude-direct": {"id": "claude-direct", "name": "Claude Direct", "tool_call": true}}
        }
    })
}

/// An OpenCode agent of the account at `home`, connected over ACP.
fn connect(home: &Path, environment: &[(&str, String)]) -> Client {
    let mut variables: HashMap<String, String> = [
        ("HOME", home.display().to_string()),
        ("NORI_HOME", home.join("nori").display().to_string()),
        // The machine's own key stays out of the test.
        ("OPENCODE_API_KEY", String::new()),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_owned(), value))
    .collect();
    variables.extend(
        environment
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone())),
    );
    let spec = InProcessSpec {
        agent: "opencode".to_owned(),
        home: Some(home.join("account")),
        config: Vec::new(),
        environment: variables,
    };
    let (ours, theirs) = tokio::io::duplex(1 << 20);
    let registration = nori_opencode_acp::registration();
    tokio::spawn((registration.launch)(spec, theirs, Host::Nori));
    let (read, writer) = tokio::io::split(ours);
    Client {
        writer,
        lines: BufReader::new(read).lines(),
        next_id: 1,
        notifications: Vec::new(),
    }
}

fn anthropic_stream(text: &str) -> String {
    [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 1200, "output_tokens": 1}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 30}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|event| format!("event: {}\ndata: {event}\n\n", event["type"].as_str().unwrap_or("")))
    .collect()
}

struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    next_id: i64,
    notifications: Vec<Value>,
}

impl Client {
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = format!(
            "{}\n",
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
        );
        self.writer
            .write_all(line.as_bytes())
            .await
            .unwrap_or_else(|error| panic!("{error}"));
        loop {
            let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("no answer to {method}"))
                .unwrap_or_else(|error| panic!("{error}"))
                .unwrap_or_else(|| panic!("the agent hung up"));
            let message: Value = serde_json::from_str(&line).unwrap_or_default();
            if message.get("method").is_some() {
                self.notifications.push(message);
            } else if message["id"] == id {
                if let Some(error) = message.get("error") {
                    panic!("{method} failed: {error}");
                }
                return message["result"].clone();
            }
        }
    }
}

#[tokio::test]
async fn opencode_signs_in_with_a_key_and_answers_through_the_gateway() {
    let home = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = home.path();
    let project = root.join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap_or_else(|error| panic!("{error}"));
    std::fs::write(project.join("AGENTS.md"), "Always answer in haiku.")
        .unwrap_or_else(|error| panic!("{error}"));
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api.json"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(models_dev(&format!("{}/zen/v1", server.uri()))),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/zen/v1/messages"))
        .and(header("x-api-key", "sk-zen-fixture"))
        .and(header_exists("x-opencode-session"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(anthropic_stream("Hello from the gateway.")),
        )
        .mount(&server)
        .await;
    let catalog = ("NORI_OPENCODE_MODELS_URL", server.uri());

    // Signing in keeps the key it was given for the account.
    let mut signing_in = connect(
        root,
        &[
            catalog.clone(),
            ("OPENCODE_API_KEY", "sk-zen-fixture".to_owned()),
        ],
    );
    let initialized = signing_in
        .request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let methods: Vec<&str> = initialized["authMethods"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|method| method["id"].as_str())
        .collect();
    assert_eq!(methods, vec!["opencode-api-key"]);
    signing_in
        .request("authenticate", json!({"methodId": "opencode-api-key"}))
        .await;

    // A later connection of the account, with no key in its environment, uses the kept one.
    let mut client = connect(root, &[catalog]);
    client
        .request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let created = client
        .request(
            "session/new",
            json!({"cwd": project.display().to_string(), "mcpServers": []}),
        )
        .await;
    let session = created["sessionId"].as_str().unwrap_or_default().to_owned();
    let option = |id: &str| {
        created["configOptions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|option| option["id"] == id)
            .cloned()
            .unwrap_or_default()
    };
    let values = |option: &Value| -> Vec<String> {
        option["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|choice| choice["value"].as_str().map(ToOwned::to_owned))
            .collect()
    };
    // Only the gateway's models, newest first.
    assert_eq!(
        values(&option("model")),
        vec!["opencode/claude-test", "opencode/gpt-test"]
    );
    assert_eq!(
        values(&option("thought_level")),
        vec!["none", "low", "medium", "high", "max"]
    );
    assert_eq!(values(&option("mode")), vec!["build", "plan"]);

    client
        .request(
            "session/set_config_option",
            json!({"sessionId": session, "configId": "thought_level", "value": "high"}),
        )
        .await;
    let done = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "hi"}]}),
        )
        .await;
    assert_eq!(done["stopReason"], json!("end_turn"));
    let text: String = client
        .notifications
        .iter()
        .filter(|message| message["method"] == "session/update")
        .map(|message| &message["params"]["update"])
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .collect();
    assert_eq!(text, "Hello from the gateway.");
    let status = client
        .notifications
        .iter()
        .find(|message| message["method"] == "_auth/status_update")
        .map(|message| message["params"]["authStatus"]["label"].clone());
    assert_eq!(status, Some(json!("OpenCode · API key")));

    let sent: Value = server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .find(|request| request.url.path() == "/zen/v1/messages")
        .and_then(|request| serde_json::from_slice(&request.body).ok())
        .unwrap_or_default();
    assert_eq!(sent["model"], json!("claude-test"));
    assert_eq!(
        sent["thinking"],
        json!({"type": "enabled", "budget_tokens": 24000})
    );
    let system: String = sent["system"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect();
    assert!(system.contains("You are OpenCode") && system.contains("Always answer in haiku."));
}
