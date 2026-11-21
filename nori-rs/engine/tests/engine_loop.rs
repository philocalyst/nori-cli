//! The engine end to end over ACP, against a fake provider: a tool round, a refused edit, a
//! mid-turn model switch and a resume from the journal.

use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use nori_acp_kit::AgentError;
use nori_acp_kit::AuthStatus;
use nori_acp_kit::async_trait;
use nori_engine::Connection;
use nori_engine::Effect;
use nori_engine::EffortStyle;
use nori_engine::EngineAgent;
use nori_engine::Flavor;
use nori_engine::Identity;
use nori_engine::ModelInfo;
use nori_engine::Rule;
use nori_engine::Workspace;
use nori_engine::tools;
use nori_llm::Auth;
use nori_llm::Endpoint;
use nori_llm::Wire;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;

/// Streams one canned answer per request, in order.
struct Script(Mutex<VecDeque<String>>);

impl Respond for Script {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let body = self
            .0
            .lock()
            .ok()
            .and_then(|mut answers| answers.pop_front())
            .unwrap_or_else(|| chat(&[json!({"content": "(no more answers)"})], "stop"));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body)
    }
}

/// A Chat Completions stream of `deltas` ending with `finish`.
fn chat(deltas: &[Value], finish: &str) -> String {
    let mut body: String = deltas
        .iter()
        .map(|delta| format!("data: {}\n\n", json!({"choices": [{"delta": delta}]})))
        .collect();
    body.push_str(&format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {}, "finish_reason": finish}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 100, "completion_tokens": 10}})
    ));
    body
}

fn tool_call(id: &str, name: &str, arguments: &Value) -> Value {
    json!({"tool_calls": [{"index": 0, "id": id, "type": "function",
        "function": {"name": name, "arguments": arguments.to_string()}}]})
}

struct TestFlavor {
    url: String,
    store: PathBuf,
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_owned(),
        label: id.to_owned(),
        description: None,
        context_window: Some(100_000),
        max_output_tokens: Some(4_000),
        efforts: Vec::new(),
        default_effort: None,
        images: false,
    }
}

#[async_trait]
impl Flavor for TestFlavor {
    fn identity(&self) -> Identity {
        Identity {
            name: "nori-test",
            title: "Test",
            store: "test",
        }
    }

    async fn workspace(&self, _cwd: &Path) -> Result<Workspace, AgentError> {
        Ok(Workspace {
            models: vec![model("fake/one"), model("fake/two")],
            default_model: Some("fake/one".to_owned()),
            rules: vec![Rule::new("edit", "*", Effect::Ask)],
            ..Workspace::default()
        })
    }

    async fn connect(&self, _cwd: &Path, model: &str) -> Result<Connection, AgentError> {
        Ok(Connection {
            endpoint: Endpoint::new(Wire::OpenAiChat, self.url.clone(), Auth::None),
            model: model.trim_start_matches("fake/").to_owned(),
            effort_style: EffortStyle::Named,
            reasoning_echo_field: None,
            extra_body: serde_json::Map::new(),
            temperature: None,
            account: None,
            output_limit: true,
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        AuthStatus::signed_out()
    }

    fn system_prompt(&self, _model: &ModelInfo) -> String {
        "You are a test agent.".to_owned()
    }

    fn tools(&self) -> Vec<Arc<dyn tools::Tool>> {
        tools::SharedTool::catalog(tools::EditStyle::Replace)
    }

    fn store_root(&self) -> PathBuf {
        self.store.clone()
    }
}

/// A minimal ACP client over the in-memory pipe.
struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    next_id: i64,
    updates: Vec<Value>,
    /// The option chosen when the agent asks for permission.
    answer: String,
}

impl Client {
    fn start(flavor: TestFlavor) -> Self {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        tokio::spawn(nori_acp_kit::serve_over_pipe(
            theirs,
            EngineAgent::new(flavor),
        ));
        let (read, writer) = tokio::io::split(ours);
        Self {
            writer,
            lines: BufReader::new(read).lines(),
            next_id: 1,
            updates: Vec::new(),
            answer: "allow_once".to_owned(),
        }
    }

    async fn send(&mut self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .unwrap_or_else(|error| panic!("write: {error}"));
    }

    async fn next(&mut self) -> Value {
        let line = tokio::time::timeout(Duration::from_secs(20), self.lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("the agent went quiet"))
            .unwrap_or_else(|error| panic!("read: {error}"))
            .unwrap_or_else(|| panic!("the agent hung up"));
        serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line}"))
    }

    /// Send a request and wait for its answer, answering the agent's own requests meanwhile.
    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.wait(id).await
    }

    async fn wait(&mut self, id: i64) -> Value {
        loop {
            let message = self.next().await;
            match (
                message.get("method").and_then(Value::as_str),
                message.get("id"),
            ) {
                (Some("session/request_permission"), Some(request)) => {
                    let request = request.clone();
                    self.updates
                        .push(json!({"permission": message["params"]["toolCall"]}));
                    let answer = self.answer.clone();
                    self.send(&json!({"jsonrpc": "2.0", "id": request,
                        "result": {"outcome": {"outcome": "selected", "optionId": answer}}}))
                        .await;
                }
                (Some(_), Some(request)) => {
                    let request = request.clone();
                    self.send(&json!({"jsonrpc": "2.0", "id": request, "result": {}}))
                        .await;
                }
                (Some(_), None) => self.updates.push(message["params"]["update"].clone()),
                (None, Some(answered)) if answered == id => {
                    if let Some(error) = message.get("error") {
                        panic!("request {id} failed: {error}");
                    }
                    return message["result"].clone();
                }
                (None, _) => {}
            }
        }
    }

    fn texts(&self) -> String {
        self.updates
            .iter()
            .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|update| update["content"]["text"].as_str())
            .collect()
    }
}

async fn requests(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|request| serde_json::from_slice(&request.body).ok())
        .collect()
}

#[tokio::test]
async fn a_tool_round_a_refused_edit_a_model_switch_and_a_resume() {
    let work = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let store = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let file = work.path().join("a.txt");
    std::fs::write(&file, "hello\n").unwrap_or_else(|error| panic!("{error}"));
    let server = MockServer::start().await;
    let answers = vec![
        chat(
            &[tool_call("call_1", "read", &json!({"file_path": "a.txt"}))],
            "tool_calls",
        ),
        chat(&[json!({"content": "It says hello."})], "stop"),
        chat(
            &[tool_call(
                "call_2",
                "edit",
                &json!({"file_path": "a.txt", "old_string": "hello", "new_string": "bye"}),
            )],
            "tool_calls",
        ),
        chat(&[json!({"content": "Resumed."})], "stop"),
    ];
    Mock::given(method("POST"))
        .respond_with(Script(Mutex::new(answers.into())))
        .mount(&server)
        .await;
    let flavor = || TestFlavor {
        url: server.uri(),
        store: store.path().to_path_buf(),
    };
    let cwd = work.path().display().to_string();

    let mut client = Client::start(flavor());
    client
        .request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let created = client
        .request("session/new", json!({"cwd": cwd, "mcpServers": []}))
        .await;
    let session = created["sessionId"].as_str().unwrap_or_default().to_owned();
    assert_eq!(
        created["configOptions"][0]["currentValue"],
        json!("fake/one")
    );

    // A tool round: the read runs, its result goes back, the model answers.
    let done = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "what is in a.txt?"}]}),
        )
        .await;
    assert_eq!(done["stopReason"], json!("end_turn"));
    assert_eq!(client.texts(), "It says hello.");
    let completed: Vec<&Value> = client
        .updates
        .iter()
        .filter(|update| update["status"] == "completed")
        .collect();
    assert_eq!(completed.len(), 1);
    assert!(
        completed[0]["content"][0]["content"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("1→hello"))
    );
    let sent = requests(&server).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[1]["messages"][3]["role"], json!("tool"));

    // The model is switched; the next request uses it.
    client
        .request(
            "session/set_config_option",
            json!({"sessionId": session, "configId": "model", "value": "fake/two"}),
        )
        .await;

    // An edit asks first; the person rejects it, so the file stays and the turn ends.
    client.answer = "reject_once".to_owned();
    let done = client
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "change it"}]}),
        )
        .await;
    assert_eq!(done["stopReason"], json!("end_turn"));
    assert!(
        client
            .updates
            .iter()
            .any(|update| update.get("permission").is_some())
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap_or_default(),
        "hello\n"
    );
    let sent = requests(&server).await;
    assert_eq!(sent[2]["model"], json!("two"));

    // A new connection resumes the session from its journal: the whole history goes out.
    let mut resumed = Client::start(flavor());
    resumed
        .request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    let options = resumed
        .request(
            "session/resume",
            json!({"sessionId": session, "cwd": cwd, "mcpServers": []}),
        )
        .await;
    assert_eq!(
        options["configOptions"][0]["currentValue"],
        json!("fake/two")
    );
    let done = resumed
        .request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": "go on"}]}),
        )
        .await;
    assert_eq!(done["stopReason"], json!("end_turn"));
    assert_eq!(resumed.texts(), "Resumed.");
    let sent = requests(&server).await;
    let roles: Vec<&str> = sent[3]["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["role"].as_str())
        .collect();
    assert_eq!(
        roles,
        vec![
            "system",
            "user",
            "assistant",
            "tool",
            "assistant",
            "user",
            "assistant",
            "tool",
            "user"
        ]
    );
    assert!(
        sent[3]["messages"][7]["content"]
            .as_str()
            .is_some_and(|text| text.contains("rejected"))
    );
}
