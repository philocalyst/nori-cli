//! Tests of the `nori-client` server: goals, tools over HTTP and the socket, context resources.

use pretty_assertions::assert_eq;
use serde_json::json;

use super::*;

fn test_agent_capabilities() -> crate::normalized::AgentCapabilitiesView {
    crate::normalized::AgentCapabilitiesView {
        http_mcp: true,
        load_session: false,
        session_list: false,
        session_resume: false,
        session_close: false,
        session_fork: false,
    }
}

#[test]
fn goal_command_enabled_by_goal_extension_without_nori_client() {
    let view = capabilities_update(test_agent_capabilities(), false, false, true, false);
    let goal = view.builtin_commands.get("goal").expect("goal command");
    assert!(goal.enabled);
    assert_eq!(goal.reason, None);
}

#[test]
fn disabled_goal_tools_do_not_hide_nori_client_capability() {
    let view = capabilities_update(test_agent_capabilities(), true, true, false, false);
    assert!(view.nori_client.advertised);
    assert!(view.nori_client.initialized);
    let goal = view.builtin_commands.get("goal").unwrap();
    assert!(!goal.enabled);
    assert_eq!(
        goal.reason.as_deref(),
        Some("Nori goal tools are disabled by session policy.")
    );
}

#[test]
fn goal_command_disabled_without_nori_client_or_goal_extension() {
    let view = capabilities_update(test_agent_capabilities(), false, false, false, false);
    let goal = view.builtin_commands.get("goal").expect("goal command");
    assert!(!goal.enabled);
    assert!(goal.reason.as_deref().is_some_and(|reason| {
        reason.contains("neither HTTP MCP support nor the goal extension")
    }));
}

fn test_orchestrator() -> super::super::orchestrator::Orchestrator {
    let (events, _events_rx) = mpsc::channel(8);
    super::super::orchestrator::Orchestrator::new(
        super::super::orchestrator::Seed::root(
            Arc::new(crate::config::NoriConfig::default()),
            "test".to_owned(),
        ),
        events,
    )
    .0
}

pub(super) fn service() -> NoriClientService {
    let (backend_event_tx, _backend_event_rx) = mpsc::channel(8);
    NoriClientService::new(NoriClientShared::new(
        Arc::new(Mutex::new(thread_goal::ThreadGoalState::default())),
        backend_event_tx,
        Arc::new(AtomicBool::new(false)),
        test_agent_capabilities(),
        true,
        false,
        test_orchestrator(),
    ))
}

fn tool_text(result: &CallToolResult) -> &str {
    result.content[0]
        .as_text()
        .expect("tool result should contain text content")
        .text
        .as_str()
}

fn is_error(result: &CallToolResult) -> bool {
    result.is_error.unwrap_or(false)
}

fn parsed_tool_text(result: &CallToolResult) -> serde_json::Value {
    serde_json::from_str(tool_text(result)).expect("tool text should be json")
}

async fn create(service: &NoriClientService, objective: &str) -> CallToolResult {
    service
        .create_goal(Parameters(CreateGoalRequest {
            objective: objective.to_string(),
        }))
        .await
        .expect("create_goal should not fail at the protocol level")
}

async fn get(service: &NoriClientService) -> CallToolResult {
    service
        .get_goal()
        .await
        .expect("get_goal should not fail at the protocol level")
}

async fn update(service: &NoriClientService, status: GoalCompletion) -> CallToolResult {
    service
        .update_goal(Parameters(UpdateGoalRequest { status }))
        .await
        .expect("update_goal should not fail at the protocol level")
}

pub(super) fn authenticated_transport(
    url: String,
    authorization_header_value: String,
) -> impl rmcp::transport::Transport<rmcp::RoleClient> {
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

    let bearer_token = authorization_header_value
        .strip_prefix("Bearer ")
        .expect("generated Authorization header should be a bearer token")
        .to_string();
    let config = StreamableHttpClientTransportConfig::with_uri(url).auth_header(bearer_token);
    rmcp::transport::StreamableHttpClientTransport::from_config(config)
}

#[test]
fn get_info_advertises_nori_client_surface() {
    let info = service().get_info();
    assert_eq!(info.server_info.name, "nori-client");
    assert!(info.capabilities.tools.is_some());
    assert!(info.capabilities.resources.is_some());
    assert!(info.capabilities.prompts.is_some());
}

#[tokio::test]
async fn get_goal_returns_null_without_goal() {
    let result = get(&service()).await;
    assert!(!is_error(&result));
    assert_eq!(parsed_tool_text(&result), json!({ "goal": null }));
}

#[tokio::test]
async fn git_tools_commit_a_batch_and_report_what_remains() {
    let git = |dir: &std::path::Path, args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
        String::from_utf8(out.stdout).expect("utf8")
    };
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().canonicalize().expect("canonical");
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.name", "Test"]);
    git(&dir, &["config", "user.email", "test@example.com"]);
    std::fs::write(dir.join("a.txt"), "a\n").expect("a");
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    std::fs::write(dir.join("a.txt"), "a2\n").expect("edit");
    std::fs::write(dir.join("b.txt"), "b\n").expect("new");

    let (backend_event_tx, _rx) = mpsc::channel(8);
    let service = NoriClientService::new(
        NoriClientShared::new(
            Arc::new(Mutex::new(thread_goal::ThreadGoalState::default())),
            backend_event_tx,
            Arc::new(AtomicBool::new(false)),
            test_agent_capabilities(),
            true,
            false,
            test_orchestrator(),
        )
        .with_workspace(Workspace {
            cwd: dir.clone(),
            workflow: Arc::default(),
        }),
    );

    let status: serde_json::Value =
        serde_json::from_str(tool_text(&service.git_status().await.expect("status")))
            .expect("json");
    assert_eq!(status["branch"], "main");
    assert_eq!(status["changes"].as_array().expect("changes").len(), 2);

    let committed = service
        .git_commit(Parameters(super::super::git_tools::GitCommitRequest {
            paths: vec!["a.txt".into()],
            message: "fix: update a".into(),
        }))
        .await
        .expect("commit");
    assert!(!is_error(&committed), "{}", tool_text(&committed));
    let body: serde_json::Value = serde_json::from_str(tool_text(&committed)).expect("json");
    assert_eq!(body["remaining_changes"], 1);
    assert_eq!(
        git(&dir, &["log", "-1", "--format=%s"]).trim(),
        "fix: update a"
    );
    assert_eq!(git(&dir, &["status", "--porcelain"]).trim(), "?? b.txt");

    let empty = service
        .git_commit(Parameters(super::super::git_tools::GitCommitRequest {
            paths: vec![],
            message: "nothing".into(),
        }))
        .await
        .expect("empty");
    assert!(is_error(&empty));
}

#[tokio::test]
async fn create_goal_creates_active_goal_and_emits_event() {
    let (backend_event_tx, mut backend_event_rx) = mpsc::channel(8);
    let service = NoriClientService::new(NoriClientShared::new(
        Arc::new(Mutex::new(thread_goal::ThreadGoalState::default())),
        backend_event_tx,
        Arc::new(AtomicBool::new(false)),
        test_agent_capabilities(),
        true,
        false,
        test_orchestrator(),
    ));

    let create_result = create(&service, "Ship the ACP goal bridge").await;
    assert!(!is_error(&create_result));
    assert!(tool_text(&create_result).contains("Ship the ACP goal bridge"));

    let emitted_event = tokio::time::timeout(
        std::time::Duration::from_millis(200),
        backend_event_rx.recv(),
    )
    .await
    .expect("create_goal should emit a client event before timeout")
    .expect("create_goal should emit a client event");
    match emitted_event {
        BackendEvent::Public(SessionEvent::Nori(nori_protocol::NoriEvent::GoalChanged(Some(
            goal,
        )))) => assert_eq!(goal.objective, "Ship the ACP goal bridge"),
        other => panic!("expected thread goal update, got {other:?}"),
    }

    let get_result = get(&service).await;
    let goal = &parsed_tool_text(&get_result)["goal"];
    assert_eq!(goal["status"], "active");
    assert_eq!(goal["objective"], "Ship the ACP goal bridge");
}

#[tokio::test]
async fn create_goal_rejects_existing_goal() {
    let service = service();
    assert!(!is_error(&create(&service, "First goal").await));

    let second = create(&service, "Second goal").await;
    assert!(is_error(&second));
    assert!(tool_text(&second).contains("already has a goal"));

    let goal = parsed_tool_text(&get(&service).await);
    assert_eq!(goal["goal"]["objective"], "First goal");
}

#[tokio::test]
async fn update_goal_marks_goal_complete() {
    let service = service();
    assert!(!is_error(&create(&service, "Finish completely").await));

    let complete = update(&service, GoalCompletion::Complete).await;
    assert!(!is_error(&complete));
    assert_eq!(parsed_tool_text(&complete)["goal"]["status"], "complete");
}

#[tokio::test]
async fn update_goal_marks_goal_blocked() {
    let service = service();
    assert!(!is_error(&create(&service, "Finish carefully").await));

    let blocked = update(&service, GoalCompletion::Blocked).await;
    assert!(!is_error(&blocked));
    assert_eq!(parsed_tool_text(&blocked)["goal"]["status"], "blocked");
}

#[tokio::test]
async fn update_goal_reports_missing_goal() {
    let result = update(&service(), GoalCompletion::Complete).await;
    assert!(is_error(&result));
    assert!(tool_text(&result).contains("no goal exists"));
}

/// End-to-end: a real spec-compliant rmcp client connects to the loopback
/// server over HTTP, exercising the streamable-HTTP transport, the
/// `initialize` handshake (which must flip `connected`), tool discovery, and
/// a `create_goal` round-trip, i.e. the path a real ACP agent takes.
/// An agent without HTTP MCP is given a stdio command that relays to a private unix socket;
/// the same service, task tools included, answers there.
#[cfg(unix)]
#[tokio::test]
async fn the_socket_serves_the_same_tools_to_agents_without_http() {
    use rmcp::ServiceExt;

    set_stdio_bridge(
        std::path::PathBuf::from("nori"),
        vec!["stdio-to-uds".to_owned()],
    );
    let (backend_event_tx, _backend_event_rx) = mpsc::channel(8);
    let server = NoriClientServer::spawn(
        NoriClientShared::new(
            Arc::new(Mutex::new(thread_goal::ThreadGoalState::default())),
            backend_event_tx,
            Arc::new(AtomicBool::new(false)),
            test_agent_capabilities(),
            true,
            false,
            test_orchestrator(),
        ),
        false,
    )
    .await
    .expect("nori-client server should spawn");
    let acp::McpServer::Stdio(stdio) = server.as_mcp_server() else {
        panic!("an agent without HTTP MCP is given a stdio server");
    };
    let socket = stdio.args.last().cloned().expect("the socket path");
    assert_eq!(stdio.args[0], "stdio-to-uds");
    let stream = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("connect to the nori-client socket");
    let client = ().serve(stream).await.expect("initialize over the socket");
    let tools = client.list_all_tools().await.expect("list tools");
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    for tool in [
        "bash",
        "delegate_task",
        "task_status",
        "task_wait",
        "task_send",
        "task_cancel",
        "get_goal",
    ] {
        assert!(names.contains(&tool), "{tool} missing from {names:?}");
    }
    for omitted in bash_tool::OPT_IN_ONLY_TOOLS {
        assert!(
            !names.contains(omitted),
            "{omitted} should not be in default {names:?}"
        );
    }
    let delegate = tools
        .iter()
        .find(|tool| tool.name == "delegate_task")
        .expect("delegate_task");
    assert!(delegate.meta.as_ref().is_some_and(
        |meta| meta.0.get("anthropic/alwaysLoad") == Some(&serde_json::Value::Bool(true))
    ));
    let _ = client.cancel().await;
    let directory = std::path::Path::new(&socket)
        .parent()
        .map(std::path::Path::to_path_buf)
        .expect("socket directory");
    drop(server);
    assert!(
        !directory.exists(),
        "the socket directory is removed with the server"
    );
}

#[tokio::test]
async fn configured_mcp_catalog_and_dispatch_use_the_same_policy() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParam;
    let (events, _rx) = mpsc::channel(8);
    let state = Arc::new(Mutex::new(thread_goal::ThreadGoalState::default()));
    let mut shared = NoriClientShared::new(
        Arc::clone(&state),
        events,
        Arc::new(AtomicBool::new(false)),
        test_agent_capabilities(),
        true,
        false,
        test_orchestrator(),
    );
    shared.tool_selection = Some(nori_host_tools::ToolSelection {
        enabled: Some(["get_goal".to_owned()].into_iter().collect()),
        descriptions: [("get_goal".to_owned(), "Read the selected goal".to_owned())]
            .into_iter()
            .collect(),
        instructions: vec!["Follow the configured prompt".to_owned()],
        ..Default::default()
    });
    let server = NoriClientServer::spawn(shared, true).await.unwrap();
    let transport = authenticated_transport(
        server.url().to_string(),
        server.authorization_header_value.clone(),
    );
    let client = ().serve(transport).await.unwrap();
    let instructions = client
        .peer()
        .peer_info()
        .unwrap()
        .instructions
        .as_ref()
        .unwrap();
    assert!(instructions.contains("Follow the configured prompt"));
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect::<Vec<_>>(),
        vec!["get_goal"]
    );
    assert_eq!(
        tools[0].description.as_deref(),
        Some("Read the selected goal")
    );
    let result = client
        .call_tool(CallToolRequestParam {
            name: "create_goal".into(),
            arguments: serde_json::json!({"objective": "must not run"})
                .as_object()
                .cloned(),
        })
        .await;
    assert!(result.is_err() || result.unwrap().is_error == Some(true));
    let goal = client
        .call_tool(CallToolRequestParam {
            name: "get_goal".into(),
            arguments: None,
        })
        .await
        .unwrap();
    assert!(goal.content.iter().any(|content| {
        content
            .as_text()
            .is_some_and(|text| text.text.contains("null"))
    }));
    let _ = client.cancel().await;
}

#[tokio::test]
async fn real_mcp_client_round_trips_over_http() {
    use rmcp::ServiceExt;
    use rmcp::model::CallToolRequestParam;

    let connected = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(thread_goal::ThreadGoalState::default()));
    let (backend_event_tx, _backend_event_rx) = mpsc::channel(8);
    let server = NoriClientServer::spawn(
        NoriClientShared::new(
            Arc::clone(&state),
            backend_event_tx,
            Arc::clone(&connected),
            test_agent_capabilities(),
            true,
            false,
            test_orchestrator(),
        ),
        true,
    )
    .await
    .expect("nori-client server should spawn");

    let transport = authenticated_transport(
        server.url().to_string(),
        server.authorization_header_value.clone(),
    );
    let client = ()
        .serve(transport)
        .await
        .expect("rmcp client should complete the initialize handshake");

    assert!(
        connected.load(Ordering::Relaxed),
        "initialize handshake must flip the connected gate"
    );
    let instructions = client
        .peer()
        .peer_info()
        .and_then(|info| info.instructions.as_deref())
        .expect("initialized nori-client instructions");
    assert!(instructions.contains(thread_goal::NORI_GOAL_CONTROL_INSTRUCTIONS));

    let tools = client
        .list_all_tools()
        .await
        .expect("client should list tools");
    let tool_names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    assert!(tool_names.contains(&"get_goal"));
    assert!(tool_names.contains(&"create_goal"));
    assert!(tool_names.contains(&"update_goal"));
    assert!(tool_names.contains(&"conversation"));
    assert!(tool_names.contains(&"conversation_context"));
    assert!(tool_names.contains(&"delegate_task"));

    let create = client
        .call_tool(CallToolRequestParam {
            name: "create_goal".into(),
            arguments: serde_json::json!({ "objective": "Round-trip over real MCP" })
                .as_object()
                .cloned(),
        })
        .await
        .expect("create_goal call should succeed");
    assert_eq!(create.is_error, Some(false));

    let get = client
        .call_tool(CallToolRequestParam {
            name: "get_goal".into(),
            arguments: None,
        })
        .await
        .expect("get_goal call should succeed");
    let goal_text = get.content[0]
        .as_text()
        .expect("get_goal returns text")
        .text
        .as_str();
    let goal: serde_json::Value = serde_json::from_str(goal_text).expect("goal json");
    assert_eq!(goal["goal"]["objective"], "Round-trip over real MCP");
    assert_eq!(goal["goal"]["status"], "active");

    client.cancel().await.expect("client should shut down");
}

#[tokio::test]
async fn real_mcp_client_discovers_context_resources_over_http() {
    use std::collections::BTreeSet;

    use rmcp::ServiceExt;
    use rmcp::model::ReadResourceRequestParam;
    use rmcp::model::ResourceContents;

    let connected = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(thread_goal::ThreadGoalState::default()));
    let (backend_event_tx, _backend_event_rx) = mpsc::channel(8);
    let server = NoriClientServer::spawn(
        NoriClientShared::new(
            Arc::clone(&state),
            backend_event_tx,
            Arc::clone(&connected),
            test_agent_capabilities(),
            true,
            false,
            test_orchestrator(),
        ),
        true,
    )
    .await
    .expect("nori-client server should spawn");

    let transport = authenticated_transport(
        server.url().to_string(),
        server.authorization_header_value.clone(),
    );
    let client = ()
        .serve(transport)
        .await
        .expect("rmcp client should complete the initialize handshake");

    let resources = client
        .list_all_resources()
        .await
        .expect("client should list resources");
    let resource_uris: BTreeSet<&str> = resources
        .iter()
        .map(|resource| resource.raw.uri.as_str())
        .collect();
    assert_eq!(
        resource_uris,
        BTreeSet::from([
            "nori://context/cli",
            "nori://context/repo",
            "nori://help/acp-wire-logs",
            "nori://help/custom-acp-agent",
        ])
    );

    let context = client
        .read_resource(ReadResourceRequestParam {
            uri: "nori://context/cli".to_string(),
        })
        .await
        .expect("client should read CLI context");
    assert_eq!(context.contents.len(), 1);
    let ResourceContents::TextResourceContents { text, .. } = &context.contents[0] else {
        panic!("CLI context should be text");
    };
    assert!(text.contains("terminal harness for agent sessions"));
    assert!(text.contains("internal-only, backend-owned MCP server"));
    assert!(text.contains("https://github.com/tilework-tech/nori-cli"));

    let repo = client
        .read_resource(ReadResourceRequestParam {
            uri: "nori://context/repo".to_string(),
        })
        .await
        .expect("client should read repo source context");
    let ResourceContents::TextResourceContents { text, .. } = &repo.contents[0] else {
        panic!("repo context should be text");
    };
    assert!(text.contains("answering Nori CLI questions"));
    assert!(text.contains("nori-rs/acp"));
    assert!(text.contains("nori-rs/tui"));

    let custom_agent = client
        .read_resource(ReadResourceRequestParam {
            uri: "nori://help/custom-acp-agent".to_string(),
        })
        .await
        .expect("client should read custom ACP agent help");
    let ResourceContents::TextResourceContents { text, .. } = &custom_agent.contents[0] else {
        panic!("custom ACP agent help should be text");
    };
    assert!(text.contains("[[agents]]"));
    assert!(text.contains("exactly one distribution block"));
    assert!(text.contains("[agents.distribution.uvx]"));

    let wire_logs = client
        .read_resource(ReadResourceRequestParam {
            uri: "nori://help/acp-wire-logs".to_string(),
        })
        .await
        .expect("client should read ACP wire logs help");
    let ResourceContents::TextResourceContents { text, .. } = &wire_logs.contents[0] else {
        panic!("ACP wire logs help should be text");
    };
    assert!(text.contains("off by default"));
    assert!(text.contains("many MB per log file"));
    assert!(text.contains("sensitive environment variables or command output"));
    assert!(text.contains("[acp_proxy]"));
    assert!(text.contains("$NORI_HOME/acp-wire"));
    assert!(text.contains("direction"));

    client.cancel().await.expect("client should shut down");
}

#[tokio::test]
async fn real_mcp_client_discovers_workflow_prompts_over_http() {
    use std::collections::BTreeSet;

    use rmcp::ServiceExt;
    use rmcp::model::GetPromptRequestParam;
    use rmcp::model::PromptMessageContent;

    let connected = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(thread_goal::ThreadGoalState::default()));
    let (backend_event_tx, _backend_event_rx) = mpsc::channel(8);
    let server = NoriClientServer::spawn(
        NoriClientShared::new(
            Arc::clone(&state),
            backend_event_tx,
            Arc::clone(&connected),
            test_agent_capabilities(),
            true,
            false,
            test_orchestrator(),
        ),
        true,
    )
    .await
    .expect("nori-client server should spawn");

    let transport = authenticated_transport(
        server.url().to_string(),
        server.authorization_header_value.clone(),
    );
    let client = ()
        .serve(transport)
        .await
        .expect("rmcp client should complete the initialize handshake");

    let prompts = client
        .list_all_prompts()
        .await
        .expect("client should list prompts");
    let prompt_names: BTreeSet<&str> = prompts.iter().map(|prompt| prompt.name.as_str()).collect();
    assert_eq!(
        prompt_names,
        BTreeSet::from([
            "answer_nori_cli_question",
            "debug_acp_wire_protocol",
            "register_custom_acp_agent",
        ])
    );

    let prompt = client
        .get_prompt(GetPromptRequestParam {
            name: "answer_nori_cli_question".to_string(),
            arguments: None,
        })
        .await
        .expect("client should get source Q&A prompt");
    assert_eq!(prompt.messages.len(), 1);
    let PromptMessageContent::Text { text } = &prompt.messages[0].content else {
        panic!("source Q&A prompt should be text");
    };
    assert!(text.contains("nori://context/repo"));
    assert!(text.contains("Nori CLI"));

    let prompt = client
        .get_prompt(GetPromptRequestParam {
            name: "debug_acp_wire_protocol".to_string(),
            arguments: None,
        })
        .await
        .expect("client should get ACP wire debug prompt");
    let PromptMessageContent::Text { text } = &prompt.messages[0].content else {
        panic!("ACP wire debug prompt should be text");
    };
    assert!(text.contains("nori://help/acp-wire-logs"));
    assert!(text.contains("timestamp-ordered request/response timeline"));
    assert!(text.contains("first divergent protocol boundary"));

    client.cancel().await.expect("client should shut down");
}
