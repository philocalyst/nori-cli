//! Translation of OpenCode streaming output into ACP updates.

use serde_json::Value;
use serde_json::json;

fn tool_kind(name: &str) -> &'static str {
    match name {
        "shell" | "bash" => "execute",
        "webfetch" => "fetch",
        "edit" | "patch" | "apply_patch" | "write" => "edit",
        "grep"
        | "glob"
        | "context"
        | "context7_resolve_library_id"
        | "context7_get_library_docs" => "search",
        "read" => "read",
        "subagent" | "task" => "think",
        _ => "other",
    }
}

pub(crate) fn update_for_event(kind: &str, data: &Value) -> Option<Value> {
    let id = data["id"].as_str().unwrap_or_default();
    match kind {
        "session.text.delta" | "session.reasoning.delta" => {
            let update = if kind == "session.text.delta" {
                "agent_message_chunk"
            } else {
                "agent_thought_chunk"
            };
            let message_id = data["assistantMessageID"].as_str()?;
            let message_id = if kind == "session.reasoning.delta" {
                format!("{message_id}:reasoning:{}", data["ordinal"])
            } else {
                message_id.to_owned()
            };
            Some(
                json!({"sessionUpdate":update,"messageId":message_id,"content":{"type":"text","text":data["delta"]}}),
            )
        }
        "session.tool.input.started" => {
            let name = data["name"].as_str().unwrap_or("Tool");
            Some(
                json!({"sessionUpdate":"tool_call","toolCallId":id,"name":name,"title":name,"kind":tool_kind(name),"status":"pending","rawInput":{}}),
            )
        }
        "session.tool.called" | "session.tool.progress" => {
            let name = data["name"].as_str().unwrap_or("Tool");
            let title = data["input"]["command"]
                .as_str()
                .filter(|_| matches!(name, "shell" | "bash"))
                .unwrap_or(name);
            Some(
                json!({"sessionUpdate":"tool_call_update","toolCallId":id,"title":title,"kind":tool_kind(name),"status":"in_progress","rawInput":data["input"]}),
            )
        }
        "session.tool.success" => {
            let content = data["content"]
                .as_array()
                .into_iter()
                .flatten()
                // ACP and MCP share content blocks. Unknown or malformed blocks stay in
                // rawOutput instead of making the whole notification invalid.
                .filter(|item| {
                    serde_json::from_value::<nori_acp_kit::schema::ContentBlock>((*item).clone())
                        .is_ok()
                })
                .map(|item| json!({"type":"content", "content":item}))
                .collect::<Vec<_>>();
            Some(json!({"sessionUpdate":"tool_call_update", "toolCallId":id,
                "status":"completed", "content":content, "rawOutput":data}))
        }
        "session.tool.failed" => Some(
            json!({"sessionUpdate":"tool_call_update","toolCallId":id,"status":"failed","content":[{"type":"content","content":{"type":"text","text":data["error"]["message"]}}],"rawOutput":data}),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn rich_and_unknown_tool_content_survives_live_updates() {
        let data = json!({"id":"call", "content":[
            {"type":"text", "text":"done"},
            {"type":"image", "data":"YWJj", "mimeType":"image/png"},
            {"type":"resource", "resource":{"uri":"file:///x", "text":"body"}},
            {"type":"audio", "data":"YWJj", "mimeType":"audio/wav"},
            {"type":"future", "data":{"x":1}}
        ], "structuredContent":{"ok":true}, "_meta":{"trace":"id"}, "futureField":7});
        let update = update_for_event("session.tool.success", &data).unwrap();
        assert_eq!(update["rawOutput"], data);
        let content = update["content"].as_array().unwrap();
        assert_eq!(content.len(), 4);
        assert_eq!(content[1]["content"], data["content"][1]);
        assert_eq!(content[2]["content"], data["content"][2]);
        assert_eq!(content[3]["content"], data["content"][3]);
        let failure =
            json!({"id":"call", "error":{"message":"failed", "code":9}, "futureField":true});
        assert_eq!(
            update_for_event("session.tool.failed", &failure).unwrap()["rawOutput"],
            failure
        );
    }
}
