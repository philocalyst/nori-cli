use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;

async fn serve_over_pipe(
    pipe: tokio::io::DuplexStream,
    _executable: PathBuf,
    environment: HashMap<OsString, OsString>,
) -> std::io::Result<()> {
    nori_acp_kit::serve_over_pipe(pipe, super::OpenCodeAgent::new(environment)).await
}

#[path = "child_lifecycle_tests.rs"]
mod child_lifecycle_tests;

async fn read_response(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    id: i64,
) -> Value {
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .unwrap_or_else(|_| panic!("response timeout for request {id}"))
            .expect("read")
            .expect("response");
        let message: Value = serde_json::from_str(&line).expect("response JSON");
        if message["id"] == id {
            return message;
        }
    }
}

#[tokio::test]
async fn acp_initialize_and_new_session_with_real_server() {
    run_real_server_test(false).await;
}

#[tokio::test]
async fn real_engine_question_reaches_acp_and_returns_answer() {
    run_real_server_test(true).await;
}

async fn run_real_server_test(question_only: bool) {
    // Native OpenCode connects to a user-owned server. These legacy tests build
    // a per-test engine process and are only meaningful when an explicit server
    // endpoint is provided by the integration harness.
    if std::env::var_os("NORI_OPENCODE_URL").is_none()
        && std::env::var_os("OPENCODE_SERVER_URL").is_none()
    {
        return;
    }
    let Ok(output) = std::process::Command::new("which").arg("opencode").output() else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let executable = PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("executable path")
            .trim(),
    );
    let Ok(version) = std::process::Command::new(&executable)
        .arg("--version")
        .output()
    else {
        return;
    };
    if !String::from_utf8_lossy(&version.stdout).contains("v2.") {
        return;
    }
    let home =
        std::env::temp_dir().join(format!("nori-opencode-acp-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).expect("temporary home");
    let additional = home.join("additional");
    std::fs::create_dir(&additional).expect("additional directory");
    let commands = home.join(".opencode/commands");
    std::fs::create_dir_all(&commands).expect("command directory");
    std::fs::write(
        commands.join("greet.md"),
        "---\ndescription: Say hello\n---\nSay hello.\n",
    )
    .expect("test command");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock model listener");
    let address = listener.local_addr().expect("mock model address");
    let marker = home.join("permission-must-not-run");
    let marker_for_model = marker.to_string_lossy().into_owned();
    let child_file = home.join("opencode.json").to_string_lossy().into_owned();
    let transient_requests = Arc::new(AtomicI64::new(0));
    let mock_transient_requests = transient_requests.clone();
    let question_answer_seen = Arc::new(AtomicBool::new(false));
    let mock_question_answer_seen = question_answer_seen.clone();
    let model_task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let marker_for_model = marker_for_model.clone();
            let child_file = child_file.clone();
            let transient_requests = mock_transient_requests.clone();
            let question_answer_seen = mock_question_answer_seen.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut bytes = [0; 8192];
                loop {
                    let Ok(count) = socket.read(&mut bytes).await else {
                        return;
                    };
                    if count == 0 {
                        return;
                    }
                    request.extend_from_slice(&bytes[..count]);
                    if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse::<i64>().ok())
                            .unwrap_or(0);
                        if i64::try_from(request.len() - end - 4).unwrap_or_default() >= length {
                            break;
                        }
                    }
                }
                let text_body = concat!(
                    "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"coder\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello from the mock model.\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"coder\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":6,\"total_tokens\":11}}\n\n",
                    "data: [DONE]\n\n"
                );
                let input = String::from_utf8_lossy(&request);
                if input.contains("question-case")
                    && input.contains("tool_call_id")
                    && input.contains("Paris")
                {
                    question_answer_seen.store(true, Ordering::SeqCst);
                }
                let user_content = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .and_then(|index| serde_json::from_slice::<Value>(&request[index + 4..]).ok())
                    .and_then(|body| body["messages"].as_array().cloned())
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|message| message["role"] == "user")
                    .map(|message| message["content"].to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                let child_work = user_content.contains("child work");
                let nested_child = user_content.contains("nested child work");
                let grandchild = user_content.contains("grandchild-check");
                if input.contains("slow child worker") && !input.contains("tool_call_id") {
                    tokio::time::sleep(Duration::from_secs(8)).await;
                }
                let error_code = if input.contains("quota-case") {
                    Some("insufficient_quota")
                } else if input.contains("usage-case") {
                    Some("usage_limit_exceeded")
                } else if input.contains("transient-case")
                    && transient_requests.fetch_add(1, Ordering::SeqCst) < 2
                {
                    Some("rate_limit_exceeded")
                } else {
                    None
                };
                if let Some(code) = error_code {
                    let body =
                        json!({"error":{"type":code,"code":code,"message":"mock provider error"}})
                            .to_string();
                    let response = format!(
                        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 0\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    return;
                }
                let body = if input.contains("child-case") && !input.contains("tool_call_id") {
                    let background = input.contains("background-child-case");
                    let child_prompt = if background || input.contains("cancel-child-case") {
                        "slow child worker"
                    } else if input.contains("nested-child-case") {
                        "nested child work"
                    } else {
                        "child work"
                    };
                    let arguments = json!({"description":"child fixture","prompt":child_prompt,"agent":"general","background":background}).to_string();
                    let call = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_child","type":"function","function":{"name":"subagent","arguments":arguments}}]},"finish_reason":null}]});
                    let finish = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
                    format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
                } else if nested_child && !input.contains("tool_call_id") {
                    let arguments = json!({"description":"grandchild fixture","prompt":"grandchild-check","agent":"general"}).to_string();
                    let call = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_nested","type":"function","function":{"name":"subagent","arguments":arguments}}]},"finish_reason":null}]});
                    let finish = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
                    format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
                } else if grandchild {
                    concat!(
                        "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"coder\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Unique grandchild response.\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"coder\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n"
                    ).to_owned()
                } else if child_work && !input.contains("tool_call_id") {
                    let arguments = json!({"path":child_file}).to_string();
                    let call = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_child_read","type":"function","function":{"name":"read","arguments":arguments}}]},"finish_reason":null}]});
                    let finish = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
                    format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
                } else if child_work {
                    concat!(
                        "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"coder\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Unique child response.\"},\"finish_reason\":null}]}\n\n",
                        "data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"coder\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                        "data: [DONE]\n\n"
                    ).to_owned()
                } else if input.contains("question-case") && !input.contains("tool_call_id") {
                    let arguments = json!({"questions":[{"header":"Location","question":"Which city?","options":[{"label":"Paris","description":"France"},{"label":"Rome","description":"Italy"}]}]}).to_string();
                    let call = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_question","type":"function","function":{"name":"question","arguments":arguments}}]},"finish_reason":null}]});
                    let finish = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
                    format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
                } else if input.contains("permission-case") && !input.contains("tool_call_id") {
                    let command =
                        json!({"command":format!("touch {marker_for_model}")}).to_string();
                    let call = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call_test","type":"function","function":{"name":"shell","arguments":command}}]},"finish_reason":null}]});
                    let finish = json!({"id":"chatcmpl-test","object":"chat.completion.chunk","created":1,"model":"coder","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
                    format!("data: {call}\n\ndata: {finish}\n\ndata: [DONE]\n\n")
                } else {
                    text_body.to_owned()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    let config = json!({"model":"local/coder","permissions":[{"action":"shell","resource":"*","effect":"ask"}],"providers":{"local":{"name":"Local test","package":"@opencode/ai/providers/openai-compatible","settings":{"baseURL":format!("http://{address}/v1"),"apiKey":"test"},"models":{"coder":{"modelID":"coder","capabilities":{"tools":true,"input":["text"],"output":["text"]},"limit":{"context":32000,"output":1024}}}}}});
    std::fs::write(home.join("opencode.json"), config.to_string())
        .expect("mock model configuration");
    let mut env = HashMap::<OsString, OsString>::new();
    env.insert("HOME".into(), home.as_os_str().into());
    env.insert("OPENCODE_CONFIG_CONTENT".into(), "{}".into());
    for (key, value) in [
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        env.insert(key.into(), home.join(value).into());
    }
    let (agent, client) = tokio::io::duplex(65536);
    let task = tokio::spawn(serve_over_pipe(agent, executable, env));
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}});
    write
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .expect("initialize request");
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("initialize timeout")
        .expect("read")
        .expect("response");
    let response: Value = serde_json::from_str(&line).expect("response JSON");
    assert_eq!(response["result"]["protocolVersion"], 1);
    let new_session = json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":home,"additionalDirectories":[additional],"mcpServers":[]}});
    write
        .write_all(format!("{new_session}\n").as_bytes())
        .await
        .expect("new session request");
    let line = tokio::time::timeout(Duration::from_secs(45), lines.next_line())
        .await
        .expect("session timeout")
        .expect("read")
        .expect("response");
    let response: Value = serde_json::from_str(&line).expect("response JSON");
    assert_eq!(response["id"], 2);
    let session_id = response["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("session ID missing: {response}"));
    let options = response["result"]["configOptions"]
        .as_array()
        .expect("config options");
    let line = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("commands timeout")
        .expect("read")
        .expect("notification");
    let commands: Value = serde_json::from_str(&line).expect("commands JSON");
    assert!(
        commands["params"]["update"]["availableCommands"]
            .as_array()
            .expect("commands")
            .iter()
            .any(|command| command["name"] == "greet"),
        "{commands}"
    );
    for config in ["model", "mode"] {
        let option = options
            .iter()
            .find(|option| option["id"] == config)
            .expect("advertised config");
        let value = option["currentValue"].as_str().expect("current value");
        let set = json!({"jsonrpc":"2.0","id":3,"method":"session/set_config_option","params":{"sessionId":session_id,"configId":config,"value":value}});
        write
            .write_all(format!("{set}\n").as_bytes())
            .await
            .expect("set config request");
        let updated = read_response(&mut lines, 3).await;
        assert_eq!(updated["id"], 3);
        assert!(updated["result"]["configOptions"].is_array(), "{updated}");
    }
    let prompt = json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"hello"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .expect("prompt request");
    let mut text = String::new();
    let mut usage_update = None;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("prompt timeout")
            .expect("read")
            .expect("response");
        let message: Value = serde_json::from_str(&line).expect("response JSON");
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            text.push_str(
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default(),
            );
        }
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "usage_update"
        {
            usage_update = Some(message["params"]["update"].clone());
        }
        if message["id"] == 5 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            assert_eq!(message["result"]["usage"]["totalTokens"], 11, "{message}");
            break;
        }
    }
    assert_eq!(text, "Hello from the mock model.");
    assert_eq!(usage_update.expect("usage update")["size"], 32000);
    let question_session = json!({"jsonrpc":"2.0","id":59,"method":"session/new","params":{"cwd":home,"mcpServers":[]}});
    write
        .write_all(format!("{question_session}\n").as_bytes())
        .await
        .unwrap();
    let question_session = read_response(&mut lines, 59).await;
    let question_session = question_session["result"]["sessionId"].as_str().unwrap();
    let question_prompt = json!({"jsonrpc":"2.0","id":60,"method":"session/prompt","params":{"sessionId":question_session,"prompt":[{"type":"text","text":"question-case"}]}});
    write
        .write_all(format!("{question_prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut question_seen = false;
    let mut question_events = Vec::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("question timeout")
            .unwrap()
            .unwrap();
        let message: Value = serde_json::from_str(&line).unwrap();
        question_events.push(message.clone());
        if message["method"] == "_nori/request_user_input" {
            question_seen = true;
            assert_eq!(message["params"]["sessionId"], question_session);
            assert_eq!(message["params"]["questions"][0]["question"], "Which city?");
            let reply = json!({"jsonrpc":"2.0","id":message["id"],"result":{"answers":{"q0":{"answers":["Paris"]}}}});
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        if message["id"] == 60 {
            assert_eq!(
                message["result"]["stopReason"], "end_turn",
                "{question_events:#?}"
            );
            break;
        }
    }
    assert!(question_seen, "OpenCode question was not delivered to ACP");
    assert!(
        question_answer_seen.load(Ordering::SeqCst),
        "the model did not receive the user's answer"
    );
    if question_only {
        drop(write);
        drop(lines);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("shutdown timeout")
            .expect("join")
            .expect("agent shutdown");
        model_task.abort();
        std::fs::remove_dir_all(home).expect("remove temporary home");
        return;
    }
    child_lifecycle_tests::exercise(&mut lines, &mut write, &home).await;
    child_lifecycle_tests::exercise_nested(&mut lines, &mut write, &home).await;
    child_lifecycle_tests::exercise_background(&mut lines, &mut write, &home).await;
    child_lifecycle_tests::exercise_cancel(&mut lines, &mut write, &home).await;
    child_lifecycle_tests::exercise_direct_child_cancel(&mut lines, &mut write, &home).await;
    child_lifecycle_tests::exercise_immediate_cancel(&mut lines, &mut write, &home).await;
    let command = json!({"jsonrpc":"2.0","id":15,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"/greet"}]}});
    write
        .write_all(format!("{command}\n").as_bytes())
        .await
        .expect("command request");
    let command_result = read_response(&mut lines, 15).await;
    assert_eq!(
        command_result["result"]["stopReason"], "end_turn",
        "{command_result}"
    );
    let compact = json!({"jsonrpc":"2.0","id":16,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"/compact"}]}});
    write
        .write_all(format!("{compact}\n").as_bytes())
        .await
        .expect("compaction request");
    let mut compaction_started = false;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("compaction timeout")
            .expect("read")
            .expect("response");
        let message: Value = serde_json::from_str(&line).expect("response JSON");
        if message["params"]["update"]["_meta"]["opencode/compaction"]["status"] == "started" {
            compaction_started = true;
        }
        if message["id"] == 16 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            break;
        }
    }
    assert!(compaction_started, "OpenCode did not report compaction");
    let load = json!({"jsonrpc":"2.0","id":4,"method":"session/load","params":{"sessionId":session_id,"cwd":home,"additionalDirectories":[additional],"mcpServers":[]}});
    write
        .write_all(format!("{load}\n").as_bytes())
        .await
        .expect("load request");
    let mut replayed = String::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(15), lines.next_line())
            .await
            .expect("load timeout")
            .expect("read")
            .expect("response");
        let loaded: Value = serde_json::from_str(&line).expect("response JSON");
        if loaded["method"] == "session/update"
            && loaded["params"]["update"]["sessionUpdate"] == "agent_message_chunk"
        {
            replayed.push_str(
                loaded["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default(),
            );
        }
        if loaded["id"] == 4 {
            assert!(loaded["result"]["configOptions"].is_array(), "{loaded}");
            break;
        }
    }
    assert!(
        replayed.contains("Hello from the mock model."),
        "{replayed}"
    );
    let list = json!({"jsonrpc":"2.0","id":6,"method":"session/list","params":{"cwd":home}});
    write
        .write_all(format!("{list}\n").as_bytes())
        .await
        .expect("list request");
    let listed = read_response(&mut lines, 6).await;
    assert!(
        listed["result"]["sessions"]
            .as_array()
            .expect("sessions")
            .iter()
            .any(|session| session["sessionId"] == session_id),
        "{listed}"
    );
    let listed_session = listed["result"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|session| session["sessionId"] == session_id)
        .unwrap();
    assert_eq!(listed_session["additionalDirectories"], json!([additional]));
    let fork = json!({"jsonrpc":"2.0","id":7,"method":"session/fork","params":{"sessionId":session_id,"cwd":home,"mcpServers":[]}});
    write
        .write_all(format!("{fork}\n").as_bytes())
        .await
        .expect("fork request");
    let forked = read_response(&mut lines, 7).await;
    let fork_id = forked["result"]["sessionId"].as_str().expect("fork ID");
    assert_ne!(fork_id, session_id);
    let resume = json!({"jsonrpc":"2.0","id":8,"method":"session/resume","params":{"sessionId":fork_id,"cwd":home,"mcpServers":[]}});
    write
        .write_all(format!("{resume}\n").as_bytes())
        .await
        .expect("resume request");
    let resumed = read_response(&mut lines, 8).await;
    assert!(resumed["result"]["configOptions"].is_array(), "{resumed}");
    let close =
        json!({"jsonrpc":"2.0","id":9,"method":"session/close","params":{"sessionId":fork_id}});
    write
        .write_all(format!("{close}\n").as_bytes())
        .await
        .expect("close request");
    let closed = read_response(&mut lines, 9).await;
    assert_eq!(closed["result"], json!({}), "{closed}");
    for request_id in [13, 14] {
        let delete = json!({"jsonrpc":"2.0","id":request_id,"method":"session/delete","params":{"sessionId":fork_id}});
        write
            .write_all(format!("{delete}\n").as_bytes())
            .await
            .expect("delete request");
        let deleted = read_response(&mut lines, request_id).await;
        assert_eq!(deleted["result"], json!({}), "{deleted}");
    }
    let prompt = json!({"jsonrpc":"2.0","id":10,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"permission-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .expect("permission prompt request");
    let mut asked = false;
    let mut shell_call = false;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("permission prompt timeout")
            .expect("read")
            .expect("response");
        let message: Value = serde_json::from_str(&line).expect("response JSON");
        if message["params"]["update"]["sessionUpdate"] == "tool_call"
            && message["params"]["update"]["name"] == "shell"
            && message["params"]["update"]["kind"] == "execute"
        {
            shell_call = true;
        }
        if message["method"] == "session/request_permission" {
            asked = true;
            let reply = json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":"reject"}}});
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .expect("permission reply");
        }
        if message["id"] == 10 {
            break;
        }
    }
    assert!(asked, "OpenCode did not ask for shell permission");
    assert!(shell_call, "OpenCode shell tool call was not identified");
    assert!(!marker.exists(), "rejected shell command ran");
    let fresh = json!({"jsonrpc":"2.0","id":11,"method":"session/new","params":{"cwd":home,"mcpServers":[]}});
    write
        .write_all(format!("{fresh}\n").as_bytes())
        .await
        .expect("new cancellation session");
    let fresh = read_response(&mut lines, 11).await;
    let cancel_id = fresh["result"]["sessionId"]
        .as_str()
        .expect("cancellation session ID");
    let prompt = json!({"jsonrpc":"2.0","id":12,"method":"session/prompt","params":{"sessionId":cancel_id,"prompt":[{"type":"text","text":"permission-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .expect("cancellation prompt request");
    let mut cancelled_permission = false;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("cancellation timeout")
            .expect("read")
            .expect("response");
        let message: Value = serde_json::from_str(&line).expect("response JSON");
        if message["method"] == "session/request_permission" {
            cancelled_permission = true;
            let cancel =
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":cancel_id}});
            write
                .write_all(format!("{cancel}\n").as_bytes())
                .await
                .expect("cancel notification");
            tokio::time::sleep(Duration::from_millis(200)).await;
            let reply = json!({"jsonrpc":"2.0","id":message["id"],"result":{"outcome":{"outcome":"selected","optionId":"always"}}});
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .expect("late permission reply");
        }
        if message["id"] == 12 {
            assert_eq!(message["result"]["stopReason"], "cancelled", "{message}");
            break;
        }
    }
    assert!(
        cancelled_permission,
        "OpenCode did not ask before cancellation"
    );
    assert!(
        !marker.exists(),
        "late permission response ran the cancelled command"
    );
    for (new_id, prompt_id, input) in [(20, 21, "quota-case"), (22, 23, "usage-case")] {
        let fresh = json!({"jsonrpc":"2.0","id":new_id,"method":"session/new","params":{"cwd":home,"mcpServers":[]}});
        write
            .write_all(format!("{fresh}\n").as_bytes())
            .await
            .expect("new quota session");
        let response = read_response(&mut lines, new_id).await;
        let session_id = response["result"]["sessionId"]
            .as_str()
            .expect("quota session ID");
        let prompt = json!({"jsonrpc":"2.0","id":prompt_id,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":input}]}});
        write
            .write_all(format!("{prompt}\n").as_bytes())
            .await
            .expect("quota prompt");
        let response = read_response(&mut lines, prompt_id).await;
        assert_eq!(
            response["error"]["data"]["code"], "usage_limit_reached",
            "{response}"
        );
    }
    let fresh = json!({"jsonrpc":"2.0","id":24,"method":"session/new","params":{"cwd":home,"mcpServers":[]}});
    write
        .write_all(format!("{fresh}\n").as_bytes())
        .await
        .expect("new rate-limit session");
    let response = read_response(&mut lines, 24).await;
    let session_id = response["result"]["sessionId"]
        .as_str()
        .expect("rate-limit session ID");
    let prompt = json!({"jsonrpc":"2.0","id":25,"method":"session/prompt","params":{"sessionId":session_id,"prompt":[{"type":"text","text":"transient-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .expect("rate-limit prompt");
    let response = read_response(&mut lines, 25).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");
    assert!(
        transient_requests.load(Ordering::SeqCst) >= 1,
        "mock provider did not return 429"
    );
    drop(write);
    drop(lines);
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .expect("shutdown timeout")
        .expect("join")
        .expect("agent shutdown");
    model_task.abort();
    std::fs::remove_dir_all(home).expect("remove temporary home");
}

#[tokio::test]
async fn new_session_sends_nested_location_and_rejects_a_mismatched_location() {
    let cwd = tempfile::tempdir().unwrap();
    let expected = std::fs::canonicalize(cwd.path()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = tokio::spawn(async move {
        let mut body = None;
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let header_end;
            loop {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    header_end = index + 4;
                    break;
                }
            }
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let size = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < header_end + size {
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                bytes.extend_from_slice(&buffer[..count]);
            }
            let is_create = bytes.starts_with(b"POST /api/session ");
            if is_create {
                body = Some(
                    serde_json::from_slice::<Value>(&bytes[header_end..header_end + size]).unwrap(),
                );
            }
            let response_body = if is_create {
                r#"{"data":{"id":"test-session","location":{"directory":"/"}}}"#
            } else {
                "{}"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        }
        body.unwrap()
    });
    let adapter = super::OpenCodeAgent::new(HashMap::from([(
        OsString::from("NORI_OPENCODE_URL"),
        OsString::from(format!("http://{address}")),
    )]));
    let request = nori_acp_kit::schema::NewSessionRequest::new(cwd.path().to_path_buf());
    let error = adapter.create(request).await.unwrap_err();
    assert!(error.to_string().contains("different directory"));
    let body = captured.await.unwrap();
    assert_eq!(body["location"], json!({"directory":expected}));
    assert!(body.get("directory").is_none());
}
