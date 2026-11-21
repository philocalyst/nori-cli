//! Codex on Nori's engine, end to end over ACP, against a fake ChatGPT backend: a private
//! `CODEX_HOME` with a fake sign-in (an expired access token and a refresh token), a fake token
//! endpoint and a fake Responses API. Nothing leaves the machine and no credential is real.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
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
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

const ACCOUNT: &str = "acct-fixture";
const NOW: i64 = 1_800_000_000;

/// `header.payload.signature`, unpadded base64url, as the sign-in's tokens are written.
fn jwt(claims: &Value) -> String {
    let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
    format!(
        "{}.{}.{}",
        encode(json!({"alg": "none", "typ": "JWT"}).to_string().as_bytes()),
        encode(claims.to_string().as_bytes()),
        encode(b"sig")
    )
}

fn id_token() -> String {
    jwt(&json!({
        "email": "you@example.com",
        "https://api.openai.com/auth": {
            "chatgpt_plan_type": "pro",
            "chatgpt_user_id": "u1",
            "chatgpt_account_id": ACCOUNT,
        },
    }))
}

/// An access token that expires at `exp` (Unix seconds); `tag` tells two apart.
fn access_token(tag: &str, exp: i64) -> String {
    jwt(&json!({"exp": exp, "jti": tag}))
}

fn far_future() -> i64 {
    // Far enough that the sign-in never counts as about to expire.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(NOW, |elapsed| {
            i64::try_from(elapsed.as_secs()).unwrap_or(NOW)
        })
        + 86_400
}

/// A Codex home as the Codex CLI leaves it after `codex login`: a configuration and a ChatGPT
/// sign-in whose access token is `access`.
fn codex_home(root: &Path, server: &MockServer, access: &str) -> std::path::PathBuf {
    let home = root.join("codex");
    std::fs::create_dir_all(&home).unwrap_or_else(|error| panic!("{error}"));
    std::fs::write(
        home.join("config.toml"),
        format!(
            "openai_base_url = \"{}/backend-api/codex\"\nmodel = \"gpt-5.5\"\nmodel_reasoning_effort = \"xhigh\"\nservice_tier = \"priority\"\n",
            server.uri()
        ),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    std::fs::write(
        home.join("auth.json"),
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": id_token(),
                "access_token": access,
                "refresh_token": "r1",
                "account_id": ACCOUNT,
                "oauth_provenance": {"issuer": server.uri(), "client_id": "app_test"},
            },
            "last_refresh": "2026-01-01T00:00:00Z",
        })
        .to_string(),
    )
    .unwrap_or_else(|error| panic!("{error}"));
    home
}

/// One Responses stream: the items in order, then `response.completed`.
fn stream(items: &[Value]) -> String {
    let mut events = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let mut added = item.clone();
        if added["type"] == "function_call" {
            added["arguments"] = json!("");
        }
        events.push(
            json!({"type": "response.output_item.added", "output_index": index, "item": added}),
        );
        if item["type"] == "message" {
            events.push(json!({"type": "response.output_text.delta", "item_id": "m", "delta": item["content"][0]["text"]}));
        }
        events.push(
            json!({"type": "response.output_item.done", "output_index": index, "item": item}),
        );
    }
    events.push(
        json!({"type": "response.completed", "response": {"id": "resp_1",
        "usage": {"input_tokens": 1000, "output_tokens": 50,
                  "input_tokens_details": {"cached_tokens": 100},
                  "output_tokens_details": {"reasoning_tokens": 10}}}}),
    );
    events
        .iter()
        .map(|event| {
            format!(
                "event: {}\ndata: {event}\n\n",
                event["type"].as_str().unwrap_or("")
            )
        })
        .collect()
}

fn call(id: &str, name: &str, arguments: &Value) -> Value {
    json!({"type": "function_call", "id": format!("fc_{id}"), "call_id": id, "name": name,
        "arguments": arguments.to_string()})
}

fn say(text: &str) -> Value {
    json!({"type": "message", "id": "m", "role": "assistant",
        "content": [{"type": "output_text", "text": text}]})
}

/// Streams one canned answer per request, in order, with the plan's usage windows in the headers.
struct Script(Mutex<VecDeque<String>>);

impl Script {
    fn new(answers: Vec<String>) -> Self {
        Self(Mutex::new(answers.into()))
    }
}

impl Respond for Script {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let body = self
            .0
            .lock()
            .ok()
            .and_then(|mut answers| answers.pop_front())
            .unwrap_or_else(|| stream(&[say("(no more answers)")]));
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .insert_header("x-codex-primary-used-percent", "42")
            .insert_header("x-codex-primary-window-minutes", "300")
            .insert_header("x-codex-primary-reset-at", "1900000000")
            .insert_header("x-codex-secondary-used-percent", "7.5")
            .insert_header("x-codex-secondary-window-minutes", "10080")
            .insert_header("x-codex-secondary-reset-after-seconds", "3600")
            .set_body_string(body)
    }
}

/// The token endpoint: the next sign-in, `r2` refresh token and an access token that lasts.
async fn mount_token_endpoint(server: &MockServer, new_access: &str) {
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id_token": id_token(),
            "access_token": new_access,
            "refresh_token": "r2",
        })))
        .mount(server)
        .await;
}

/// The model catalog is unreachable, which leaves Codex's bundled catalog in charge.
async fn mount_no_catalog(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/backend-api/codex/models"))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

struct Client {
    writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    lines: tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    next_id: i64,
    notifications: Vec<Value>,
}

impl Client {
    fn start(root: &Path, home: &Path) -> Self {
        let environment: HashMap<String, String> = [
            ("HOME", root.display().to_string()),
            ("NORI_HOME", root.join("nori").display().to_string()),
            // The machine's own keys stay out of the test.
            ("OPENAI_API_KEY", String::new()),
            ("CODEX_API_KEY", String::new()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        let spec = InProcessSpec {
            agent: "codex".to_owned(),
            home: Some(home.to_path_buf()),
            config: Vec::new(),
            environment,
        };
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let registration = nori_codex_acp::registration();
        tokio::spawn((registration.launch)(spec, theirs, Host::Nori));
        let (read, writer) = tokio::io::split(ours);
        Self {
            writer,
            lines: BufReader::new(read).lines(),
            next_id: 1,
            notifications: Vec::new(),
        }
    }

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
            let line = tokio::time::timeout(Duration::from_secs(30), self.lines.next_line())
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

    async fn initialize(&mut self) {
        self.request(
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    }

    async fn set(&mut self, session: &str, option: &str, value: &str) -> Value {
        self.request(
            "session/set_config_option",
            json!({"sessionId": session, "configId": option, "value": value}),
        )
        .await
    }

    async fn prompt(&mut self, session: &str, text: &str) -> Value {
        self.request(
            "session/prompt",
            json!({"sessionId": session, "prompt": [{"type": "text", "text": text}]}),
        )
        .await
    }

    fn updates(&self) -> Vec<&Value> {
        self.notifications
            .iter()
            .filter(|message| message["method"] == "session/update")
            .map(|message| &message["params"]["update"])
            .collect()
    }
}

fn option(options: &Value, id: &str) -> Value {
    options["configOptions"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|option| option["id"] == id)
        .cloned()
        .unwrap_or_default()
}

fn values(option: &Value) -> Vec<String> {
    option["options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|choice| choice["value"].as_str().map(ToOwned::to_owned))
        .collect()
}

/// What the Responses endpoint received, in order.
async fn responses(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/backend-api/codex/responses")
        .collect()
}

fn body(request: &Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or_default()
}

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| tool["name"].as_str().map(ToOwned::to_owned))
        .collect()
}

fn header_of(request: &Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

#[tokio::test]
async fn codex_runs_on_the_engine_with_its_own_sign_in_config_and_catalog() {
    let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap_or_else(|error| panic!("{error}"));
    std::fs::write(project.join("AGENTS.md"), "Always answer in haiku.")
        .unwrap_or_else(|error| panic!("{error}"));
    let server = MockServer::start().await;
    // The access token is long expired, so the first request waits for a refresh.
    let home = codex_home(root, &server, &access_token("old", 1_000_000_000));
    let fresh = access_token("fresh", far_future());
    mount_token_endpoint(&server, &fresh).await;
    mount_no_catalog(&server).await;
    let patch = "*** Begin Patch\n*** Add File: made.txt\n+made by apply_patch\n*** End Patch";
    let answers = vec![
        stream(&[call("call_1", "apply_patch", &json!({"input": patch}))]),
        stream(&[call(
            "call_2",
            "shell_command",
            &json!({"command": "cat made.txt"}),
        )]),
        stream(&[say("Done.")]),
        stream(&[say("Second.")]),
        stream(&[say("Third.")]),
        stream(&[say("Fourth.")]),
        stream(&[say("Fifth.")]),
    ];
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .respond_with(Script::new(answers))
        .mount(&server)
        .await;

    let mut client = Client::start(root, &home);
    client.initialize().await;
    let created = client
        .request(
            "session/new",
            json!({"cwd": project.display().to_string(), "mcpServers": []}),
        )
        .await;
    let session = created["sessionId"].as_str().unwrap_or_default().to_owned();
    assert_eq!(option(&created, "model")["currentValue"], json!("gpt-5.5"));
    assert_eq!(
        option(&created, "thought_level")["currentValue"],
        json!("xhigh")
    );
    assert_eq!(
        values(&option(&created, "thought_level")),
        vec!["low", "medium", "high", "xhigh"]
    );
    assert_eq!(
        option(&created, "service_tier")["currentValue"],
        json!("priority")
    );
    assert_eq!(
        values(&option(&created, "service_tier")),
        vec!["default", "priority", "flex"]
    );
    assert_eq!(
        option(&created, "mode")["currentValue"],
        json!("full-access")
    );
    assert_eq!(
        values(&option(&created, "mode")),
        vec!["read-only", "auto", "full-access"]
    );

    // A turn with two tool rounds: an expired token is refreshed first; apply_patch creates a
    // file; shell_command reads it back.
    let done = client.prompt(&session, "make a file and read it").await;
    assert_eq!(done["stopReason"], json!("end_turn"));
    assert_eq!(
        std::fs::read_to_string(project.join("made.txt")).unwrap_or_default(),
        "made by apply_patch\n"
    );
    let tokens: Vec<Request> = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/oauth/token")
        .collect();
    assert_eq!(tokens.len(), 1);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("auth.json")).unwrap_or_default())
            .unwrap_or_default();
    assert_eq!(
        (
            &stored["tokens"]["access_token"],
            &stored["tokens"]["refresh_token"]
        ),
        (&json!(fresh), &json!("r2"))
    );

    let sent = responses(&server).await;
    assert_eq!(sent.len(), 3);
    for request in &sent {
        assert_eq!(
            header_of(request, "authorization"),
            format!("Bearer {fresh}")
        );
        assert_eq!(header_of(request, "chatgpt-account-id"), ACCOUNT);
        assert_eq!(header_of(request, "originator"), "codex_cli_rs");
        assert_eq!(header_of(request, "session_id"), session);
        assert_eq!(header_of(request, "x-client-request-id"), session);
    }
    let first = body(&sent[0]);
    assert_eq!(first["model"], json!("gpt-5.5"));
    assert_eq!(first["reasoning"]["effort"], json!("xhigh"));
    assert_eq!(first["service_tier"], json!("priority"));
    assert_eq!(first["store"], json!(false));
    assert_eq!(first.get("max_output_tokens"), None);
    assert_eq!(
        tool_names(&first),
        vec![
            "shell_command",
            "read",
            "apply_patch",
            "update_plan",
            "view_image"
        ]
    );
    assert!(
        first["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("Always answer in haiku."))
    );
    let output = |request: &Request, id: &str| {
        body(request)["input"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["type"] == "function_call_output" && item["call_id"] == id)
            .map(|item| item["output"].to_string())
            .unwrap_or_default()
    };
    assert!(
        output(&sent[1], "call_1").contains("made.txt"),
        "{}",
        output(&sent[1], "call_1")
    );
    assert!(
        output(&sent[2], "call_2").contains("made by apply_patch"),
        "{}",
        output(&sent[2], "call_2")
    );

    // The plan's usage windows ride on the turn's usage update.
    let windows: Vec<Value> = client
        .updates()
        .into_iter()
        .filter(|update| update["sessionUpdate"] == "usage_update")
        .flat_map(|update| {
            update["_meta"]["nori"]["rateLimits"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect();
    assert!(!windows.is_empty());
    assert_eq!(
        (
            &windows[0]["id"],
            &windows[0]["usedPercent"],
            &windows[0]["windowMinutes"]
        ),
        (&json!("five_hour"), &json!(42.0), &json!(300))
    );
    assert_eq!(
        (
            &windows[1]["id"],
            &windows[1]["usedPercent"],
            &windows[1]["windowMinutes"]
        ),
        (&json!("seven_day"), &json!(7.5), &json!(10080))
    );
    let account = client
        .updates()
        .into_iter()
        .find_map(|update| update["_meta"]["nori"]["account"].as_str())
        .map(ToOwned::to_owned);
    assert_eq!(account.as_deref(), Some("you@example.com"));

    // A model with more efforts takes `max`; switching back clamps it to the nearest the model
    // takes, and the tier follows the model.
    let luna = client.set(&session, "model", "gpt-6-luna").await;
    assert_eq!(
        option(&luna, "thought_level")["currentValue"],
        json!("xhigh")
    );
    let luna = client.set(&session, "thought_level", "max").await;
    assert_eq!(option(&luna, "thought_level")["currentValue"], json!("max"));
    let back = client.set(&session, "model", "gpt-5.5").await;
    assert_eq!(
        option(&back, "thought_level")["currentValue"],
        json!("xhigh")
    );
    assert_eq!(
        option(&back, "service_tier")["currentValue"],
        json!("priority")
    );
    client.prompt(&session, "again").await;
    let fourth = body(&responses(&server).await[3]);
    assert_eq!(fourth["model"], json!("gpt-5.5"));
    assert_eq!(fourth["reasoning"]["effort"], json!("xhigh"));
    assert_eq!(fourth["service_tier"], json!("priority"));

    // The service tier reaches the body; the standard tier sends none.
    client.set(&session, "service_tier", "flex").await;
    client.prompt(&session, "flex").await;
    client.set(&session, "service_tier", "default").await;
    client.prompt(&session, "standard").await;
    let sent = responses(&server).await;
    assert_eq!(body(&sent[4])["service_tier"], json!("flex"));
    assert_eq!(body(&sent[5]).get("service_tier"), None);

    // Read-only mode refuses edits, so the model is not offered the patch tool.
    client.set(&session, "mode", "read-only").await;
    client.prompt(&session, "look only").await;
    let sent = responses(&server).await;
    assert_eq!(
        tool_names(&body(&sent[6])),
        vec!["shell_command", "read", "update_plan", "view_image"]
    );

    // The journal lands under Nori's own home, not Codex's.
    assert!(root.join("nori/agents/codex").is_dir());
}

#[tokio::test]
async fn a_refused_token_is_refreshed_once_and_the_request_sent_again() {
    let temp = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
    let root = temp.path();
    let project = root.join("project");
    std::fs::create_dir_all(project.join(".git")).unwrap_or_else(|error| panic!("{error}"));
    let server = MockServer::start().await;
    // The token has hours left, so only the backend's refusal says it is no good.
    let stale = access_token("stale", far_future());
    let fresh = access_token("fresh", far_future() + 1);
    let home = codex_home(root, &server, &stale);
    mount_token_endpoint(&server, &fresh).await;
    mount_no_catalog(&server).await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {stale}").as_str()))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"error": {"message": "Provided authentication token is expired.", "code": "token_expired"}})),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/responses"))
        .and(header("authorization", format!("Bearer {fresh}").as_str()))
        .respond_with(Script::new(vec![stream(&[say("Back in business.")])]))
        .mount(&server)
        .await;

    let mut client = Client::start(root, &home);
    client.initialize().await;
    let created = client
        .request(
            "session/new",
            json!({"cwd": project.display().to_string(), "mcpServers": []}),
        )
        .await;
    let session = created["sessionId"].as_str().unwrap_or_default().to_owned();
    let done = client.prompt(&session, "hello").await;
    assert_eq!(done["stopReason"], json!("end_turn"));
    let text: String = client
        .updates()
        .into_iter()
        .filter(|update| update["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|update| update["content"]["text"].as_str())
        .collect();
    assert_eq!(text, "Back in business.");

    let sent = responses(&server).await;
    let bearers: Vec<String> = sent
        .iter()
        .map(|request| header_of(request, "authorization"))
        .collect();
    assert_eq!(
        bearers,
        vec![format!("Bearer {stale}"), format!("Bearer {fresh}")]
    );
    let refreshes = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path() == "/oauth/token")
        .count();
    assert_eq!(refreshes, 1);
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(home.join("auth.json")).unwrap_or_default())
            .unwrap_or_default();
    assert_eq!(stored["tokens"]["refresh_token"], json!("r2"));
}
