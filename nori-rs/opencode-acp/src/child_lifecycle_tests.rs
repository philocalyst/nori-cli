//! Native OpenCode child lifecycle exercised through the ACP pipe.

use super::*;
use pretty_assertions::assert_eq;

pub(super) async fn exercise(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    cwd: &std::path::Path,
) {
    let new = json!({"jsonrpc":"2.0","id":29,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{new}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 29).await;
    let parent = response["result"]["sessionId"]
        .as_str()
        .expect("session ID");
    let unknown = json!({"jsonrpc":"2.0","id":41,"method":"session/prompt","params":{"sessionId":"unknown-child","prompt":[{"type":"text","text":"hello"}]}});
    write
        .write_all(format!("{unknown}\n").as_bytes())
        .await
        .unwrap();
    let rejected = read_response(lines, 41).await;
    assert!(rejected["error"].is_object(), "{rejected}");
    let prompt = json!({"jsonrpc":"2.0","id":30,"method":"session/prompt","params":{"sessionId":parent,"prompt":[{"type":"text","text":"child-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut child_id = None;
    let mut child_text = String::new();
    let mut completed = false;
    let mut child_config = None;
    let mut child_tool_started = false;
    let mut child_tool_completed = false;
    let mut parent_text = String::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("child prompt timeout")
            .expect("read")
            .expect("message");
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
        {
            let params = &message["params"];
            assert_eq!(params["sessionId"], parent);
            child_id = params["update"]["sessionId"]
                .as_str()
                .map(ToOwned::to_owned);
            if params["update"]["state"]["state"] == "idle" {
                completed = true;
            }
        }
        if message["method"] == "session/update"
            && child_id.as_deref() == message["params"]["sessionId"].as_str()
        {
            assert!(
                message["params"]["update"]
                    .get("_meta")
                    .and_then(|meta| meta.get("opencode/child-session"))
                    .is_none()
            );
            child_text.push_str(
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default(),
            );
        }
        if message["method"] == "session/update"
            && child_id.as_deref() == message["params"]["sessionId"].as_str()
        {
            match message["params"]["update"]["sessionUpdate"].as_str() {
                Some("tool_call") => child_tool_started = true,
                Some("tool_call_update")
                    if message["params"]["update"]["status"] == "completed" =>
                {
                    child_tool_completed = true;
                    assert!(!completed, "tool completion must not end the child");
                }
                _ => {}
            }
        }
        if message["method"] == "session/update" && message["params"]["sessionId"] == parent {
            parent_text.push_str(
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default(),
            );
        }
        if message["method"] == "session/update"
            && message["params"]["sessionId"] != parent
            && message["params"]["update"]["sessionUpdate"] == "config_option_update"
        {
            child_config = Some(message.clone());
        }
        if message["id"] == 30 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            break;
        }
    }
    let child_id = child_id.expect("OpenCode did not announce the child session");
    if child_config.is_none() {
        loop {
            let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
                .await
                .expect("child config timeout")
                .expect("read")
                .expect("message");
            let message: Value = serde_json::from_str(&line).unwrap();
            if message["method"] == "session/update"
                && message["params"]["sessionId"] == child_id
                && message["params"]["update"]["sessionUpdate"] == "config_option_update"
            {
                child_config = Some(message);
                break;
            }
        }
    }
    let child_config = child_config.unwrap();
    assert_eq!(child_config["params"]["sessionId"], child_id);
    assert!(
        child_config["params"]["update"]["configOptions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|option| option["id"] == "model"),
        "{child_config}"
    );
    assert!(completed, "child did not complete");
    assert!(
        child_tool_started && child_tool_completed,
        "child tool updates missing"
    );
    assert!(
        child_text.contains("Unique child response."),
        "{child_text}"
    );
    assert!(
        !parent_text.contains("Unique child response."),
        "child text leaked into parent"
    );

    // Discovered native children are directly controllable; loading is a
    // separate replay operation and must not be required before a follow-up.
    let set = json!({"jsonrpc":"2.0","id":38,"method":"session/set_config_option","params":{"sessionId":child_id,"configId":"model","value":"local/coder"}});
    write
        .write_all(format!("{set}\n").as_bytes())
        .await
        .unwrap();
    let configured = read_response(lines, 38).await;
    assert!(
        configured["result"]["configOptions"].is_array(),
        "{configured}"
    );
    let followup = json!({"jsonrpc":"2.0","id":32,"method":"session/prompt","params":{"sessionId":child_id,"prompt":[{"type":"text","text":"child followup"}]}});
    write
        .write_all(format!("{followup}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 32).await;
    assert_eq!(response["result"]["stopReason"], "end_turn", "{response}");

    let load = json!({"jsonrpc":"2.0","id":31,"method":"session/load","params":{"sessionId":child_id,"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{load}\n").as_bytes())
        .await
        .unwrap();
    let mut replay = String::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("child load timeout")
            .expect("read")
            .expect("message");
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "session/update" {
            replay.push_str(
                message["params"]["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default(),
            );
        }
        if message["id"] == 31 {
            assert!(message["result"]["configOptions"].is_array(), "{message}");
            break;
        }
    }
    assert!(replay.contains("Unique child response."), "{replay}");
}

pub(super) async fn exercise_nested(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    cwd: &std::path::Path,
) {
    let new = json!({"jsonrpc":"2.0","id":44,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{new}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 44).await;
    let parent = response["result"]["sessionId"].as_str().unwrap();
    let prompt = json!({"jsonrpc":"2.0","id":45,"method":"session/prompt","params":{"sessionId":parent,"prompt":[{"type":"text","text":"nested-child-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut child = None::<String>;
    let mut grandchild = None::<String>;
    let mut grandchild_text = String::new();
    let mut parent_text = String::new();
    let mut observed = Vec::new();
    loop {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("nested child timeout")
            .unwrap()
            .unwrap();
        let message: Value = serde_json::from_str(&line).unwrap();
        observed.push(message.clone());
        if message["method"] == "session/update" {
            let session = message["params"]["sessionId"].as_str().unwrap_or_default();
            let update = &message["params"]["update"];
            if update["sessionUpdate"] == "subagent_update" {
                let id = update["sessionId"].as_str().unwrap().to_owned();
                if session == parent {
                    child = Some(id);
                } else if child.as_deref() == Some(session) {
                    grandchild = Some(id);
                }
            }
            if grandchild.as_deref() == Some(session) {
                grandchild_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
            if session == parent {
                parent_text.push_str(update["content"]["text"].as_str().unwrap_or_default());
            }
        }
        if message["id"] == 45 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            break;
        }
    }
    let child = child.expect("missing child announcement");
    let child_announcement = observed
        .iter()
        .position(|message| {
            message["method"] == "session/update"
                && message["params"]["sessionId"] == parent
                && message["params"]["update"]["sessionUpdate"] == "subagent_update"
                && message["params"]["update"]["sessionId"] == child
        })
        .expect("child announcement event");
    let first_child_content = observed
        .iter()
        .position(|message| {
            message["method"] == "session/update"
                && message["params"]["sessionId"] == child
                && message["params"]["update"]["content"]["text"]
                    .as_str()
                    .is_some_and(|text| !text.is_empty())
        })
        .expect("child content event");
    assert!(
        child_announcement < first_child_content,
        "child content must not arrive before its parent has announced the child: {observed:#?}"
    );
    assert!(
        grandchild.is_some(),
        "missing nested announcement on its immediate parent: {observed:#?}"
    );
    assert!(
        grandchild_text.contains("Unique grandchild response."),
        "{grandchild_text}"
    );
    assert!(!parent_text.contains("Unique grandchild response."));
}

pub(super) async fn exercise_background(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    cwd: &std::path::Path,
) {
    let new = json!({"jsonrpc":"2.0","id":33,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{new}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 33).await;
    let parent = response["result"]["sessionId"]
        .as_str()
        .expect("session ID");
    let prompt = json!({"jsonrpc":"2.0","id":34,"method":"session/prompt","params":{"sessionId":parent,"prompt":[{"type":"text","text":"background-child-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut child = None;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("background prompt timeout")
            .expect("read")
            .expect("message");
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
        {
            assert_ne!(
                message["params"]["update"]["state"]["state"], "idle",
                "background child completed before parent returned"
            );
            child = message["params"]["update"]["sessionId"]
                .as_str()
                .map(ToOwned::to_owned);
        }
        if message["id"] == 34 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            break;
        }
    }
    let child = child.expect("background child not announced");
    let second = json!({"jsonrpc":"2.0","id":35,"method":"session/prompt","params":{"sessionId":parent,"prompt":[{"type":"text","text":"parent followup"}]}});
    write
        .write_all(format!("{second}\n").as_bytes())
        .await
        .unwrap();
    let mut parent_done = false;
    let mut child_done = false;
    while !parent_done || !child_done {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("background child timeout")
            .expect("read")
            .expect("message");
        let message: Value = serde_json::from_str(&line).unwrap();
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
            && message["params"]["update"]["sessionId"] == child
            && message["params"]["update"]["state"]["state"] == "idle"
        {
            child_done = true;
        }
        if message["id"] == 35 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            parent_done = true;
        }
    }
}

pub(super) async fn exercise_cancel(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    cwd: &std::path::Path,
) {
    let new = json!({"jsonrpc":"2.0","id":36,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{new}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 36).await;
    let parent = response["result"]["sessionId"]
        .as_str()
        .expect("session ID");
    let prompt = json!({"jsonrpc":"2.0","id":37,"method":"session/prompt","params":{"sessionId":parent,"prompt":[{"type":"text","text":"cancel-child-case"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut cancelled = false;
    let mut parent_done = false;
    let mut child_interrupted = false;
    loop {
        let line = tokio::time::timeout(Duration::from_secs(20), lines.next_line())
            .await
            .expect("child cancellation timeout")
            .expect("read")
            .expect("message");
        let message: Value = serde_json::from_str(&line).unwrap();
        if !cancelled
            && message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
            && message["params"]["update"]["state"]["state"] == "running"
        {
            let cancel =
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":parent}});
            write
                .write_all(format!("{cancel}\n").as_bytes())
                .await
                .unwrap();
            cancelled = true;
        }
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
            && message["params"]["update"]["state"]["state"] == "cancelled"
        {
            child_interrupted = true;
        }
        if message["id"] == 37 {
            assert!(
                cancelled,
                "child did not start before parent ended: {message}"
            );
            assert_eq!(message["result"]["stopReason"], "cancelled", "{message}");
            parent_done = true;
        }
        if parent_done && child_interrupted {
            break;
        }
    }
}

pub(super) async fn exercise_direct_child_cancel(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    cwd: &std::path::Path,
) {
    let new = json!({"jsonrpc":"2.0","id":39,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{new}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 39).await;
    let parent = response["result"]["sessionId"]
        .as_str()
        .expect("session ID");
    let prompt = json!({"jsonrpc":"2.0","id":40,"method":"session/prompt","params":{"sessionId":parent,"prompt":[{"type":"text","text":"background-child-case direct cancellation"}]}});
    write
        .write_all(format!("{prompt}\n").as_bytes())
        .await
        .unwrap();
    let mut cancelled = false;
    let mut parent_done = false;
    let mut child_interrupted = false;
    while !parent_done || !child_interrupted {
        let line = tokio::time::timeout(Duration::from_secs(30), lines.next_line())
            .await
            .expect("direct child cancel timeout")
            .expect("read")
            .expect("message");
        let message: Value = serde_json::from_str(&line).unwrap();
        if !cancelled
            && message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
            && message["params"]["update"]["state"]["state"] == "running"
        {
            let child = message["params"]["update"]["sessionId"]
                .as_str()
                .expect("child ID");
            let cancel =
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":child}});
            write
                .write_all(format!("{cancel}\n").as_bytes())
                .await
                .unwrap();
            cancelled = true;
        }
        if message["method"] == "session/update"
            && message["params"]["update"]["sessionUpdate"] == "subagent_update"
            && message["params"]["update"]["state"]["state"] == "cancelled"
        {
            child_interrupted = true;
        }
        if message["id"] == 40 {
            assert_eq!(message["result"]["stopReason"], "end_turn", "{message}");
            parent_done = true;
        }
    }
    assert!(cancelled);
}

pub(super) async fn exercise_immediate_cancel(
    lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
    cwd: &std::path::Path,
) {
    let new = json!({"jsonrpc":"2.0","id":42,"method":"session/new","params":{"cwd":cwd,"mcpServers":[]}});
    write
        .write_all(format!("{new}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 42).await;
    let session = response["result"]["sessionId"]
        .as_str()
        .expect("session ID");
    let prompt = json!({"jsonrpc":"2.0","id":43,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"slow child worker"}]}});
    let cancel = json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}});
    write
        .write_all(format!("{prompt}\n{cancel}\n").as_bytes())
        .await
        .unwrap();
    let response = read_response(lines, 43).await;
    assert_eq!(response["result"]["stopReason"], "cancelled", "{response}");
}
