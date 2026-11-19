//! Reconstruct ACP history from OpenCode's paginated v2 messages.

use std::collections::HashSet;
use std::path::Path;

use acp::UntypedMessage;
use base64::Engine;
use nori_acp_kit::Cx;
use nori_acp_kit::acp;
use reqwest::Method;
use serde_json::Value;
use serde_json::json;

use crate::OpenCodeServer;

pub(crate) async fn history(
    server: &OpenCodeServer,
    client: &Cx,
    id: &str,
    cwd: &Path,
) -> acp::Result<()> {
    let mut cursor = None::<String>;
    let mut seen = HashSet::new();
    loop {
        let query = if let Some(cursor) = &cursor {
            vec![("limit", "200"), ("cursor", cursor.as_str())]
        } else {
            vec![("limit", "200"), ("order", "asc")]
        };
        let page = server
            .request_with_query(Method::GET, &["session", id, "message"], &query, None)
            .await
            .map_err(|error| acp::Error::internal_error().data(error.to_string()))?;
        for message in page["data"]
            .as_array()
            .ok_or_else(|| acp::Error::internal_error().data("Invalid OpenCode message page"))?
        {
            for update in updates(message, cwd)? {
                client.send_notification(UntypedMessage::new(
                    "session/update",
                    json!({"sessionId":id,"update":update}),
                )?)?;
            }
        }
        cursor = page["cursor"]["next"].as_str().map(ToOwned::to_owned);
        let Some(next) = &cursor else { break };
        if !seen.insert(next.clone()) {
            return Err(acp::Error::internal_error().data("OpenCode repeated a history cursor"));
        }
    }
    Ok(())
}

fn updates(message: &Value, cwd: &Path) -> acp::Result<Vec<Value>> {
    let id = message["id"]
        .as_str()
        .ok_or_else(|| acp::Error::internal_error().data("OpenCode history message has no id"))?;
    let mut updates = Vec::new();
    match message["type"].as_str() {
        Some("user") => {
            updates.push(json!({"sessionUpdate":"user_message_chunk","messageId":id,"content":{"type":"text","text":message["text"]}}));
            for file in message["files"].as_array().into_iter().flatten() {
                let name = file["name"].as_str().unwrap_or("file");
                let mime = file["mime"].as_str().unwrap_or("application/octet-stream");
                let content = if let Some(uri) = file["source"]["uri"].as_str() {
                    json!({"type":"resource_link","uri":uri,"name":name,"mimeType":mime})
                } else {
                    let data = file["data"].as_str().unwrap_or_default();
                    let uri = reqwest::Url::from_file_path(cwd.join(name))
                        .map_err(|_| acp::Error::internal_error().data("Invalid replay file path"))?
                        .to_string();
                    if mime.starts_with("image/") {
                        json!({"type":"image","mimeType":mime,"data":data,"uri":uri})
                    } else if mime.starts_with("text/") || mime == "application/json" {
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .map_err(acp::Error::into_internal_error)?;
                        json!({"type":"resource","resource":{"uri":uri,"mimeType":mime,"text":String::from_utf8_lossy(&decoded)}})
                    } else {
                        json!({"type":"resource","resource":{"uri":uri,"mimeType":mime,"blob":data}})
                    }
                };
                updates.push(
                    json!({"sessionUpdate":"user_message_chunk","messageId":id,"content":content}),
                );
            }
        }
        Some("assistant") => {
            let mut ordinal = 0;
            for part in message["content"].as_array().into_iter().flatten() {
                match part["type"].as_str() {
                    Some("text") => updates.push(json!({"sessionUpdate":"agent_message_chunk","messageId":id,"content":{"type":"text","text":part["text"]}})),
                    Some("reasoning") => {
                        updates.push(json!({"sessionUpdate":"agent_thought_chunk","messageId":format!("{id}:reasoning:{ordinal}"),"content":{"type":"text","text":part["text"]}}));
                        ordinal += 1;
                    }
                    Some("tool") => {
                        let tool_id = part["id"].as_str().ok_or_else(|| acp::Error::internal_error().data("OpenCode tool has no id"))?;
                        let name = part["name"].as_str().unwrap_or("Tool");
                        let state = &part["state"];
                        let status = state["status"].as_str().unwrap_or("streaming");
                        let mut call = crate::event::update_for_event("session.tool.input.started", &json!({"id":tool_id,"name":name})).ok_or_else(acp::Error::internal_error)?;
                        if status != "streaming" { call["rawInput"] = state["input"].clone(); }
                        updates.push(call);
                        if name == "question" && matches!(status, "running" | "streaming") {
                            let questions = state["input"]["questions"].as_array().into_iter().flatten()
                                .filter_map(|question| question["question"].as_str())
                                .collect::<Vec<_>>().join("\n");
                            let explanation = if questions.is_empty() {
                                "The previous question was interrupted when the session reopened. You can continue the conversation.".to_owned()
                            } else {
                                format!("The previous question was interrupted when the session reopened. You can answer in the conversation:\n{questions}")
                            };
                            updates.push(json!({"sessionUpdate":"tool_call_update","toolCallId":tool_id,"status":"failed","rawInput":state["input"],"content":[{"type":"content","content":{"type":"text","text":explanation}}]}));
                            updates.push(json!({"sessionUpdate":"agent_message_chunk","messageId":format!("{id}:interrupted-question:{tool_id}"),"content":{"type":"text","text":explanation}}));
                            continue;
                        }
                        let mut result = state.clone();
                        if let Some(fields) = result.as_object_mut() {
                            fields.insert("id".to_owned(), json!(tool_id));
                        }
                        let mapped = match status {
                            "streaming" => None,
                            "running" => crate::event::update_for_event("session.tool.called", &json!({"id":tool_id,"name":name,"input":state["input"]})),
                            "completed" => crate::event::update_for_event("session.tool.success", &result),
                            "error" => crate::event::update_for_event("session.tool.failed", &result),
                            _ => return Err(acp::Error::internal_error().data("Unknown OpenCode tool status")),
                        };
                        if let Some(update) = mapped { updates.push(update); }
                    }
                    _ => return Err(acp::Error::internal_error().data("Unknown OpenCode message content")),
                }
            }
        }
        Some("synthetic") => {}
        Some("compaction") => {
            let status = match message["status"].as_str() {
                Some("completed") => "completed",
                Some("failed") => "failed",
                _ => return Ok(updates),
            };
            let mut marker = json!({"status":status,"messageId":id,"reason":message["reason"]});
            if status == "failed" {
                marker["error"] = message["error"].clone();
            }
            updates.push(json!({"sessionUpdate":"session_info_update","_meta":{"opencode/compaction":marker}}));
        }
        _ => {}
    }
    Ok(updates)
}

#[cfg(test)]
mod tests {
    use super::updates;
    use pretty_assertions::assert_eq;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn interrupted_question_is_visible_and_not_left_running() {
        let input = json!({"questions":[{"question":"Which city?","options":[{"label":"Paris"}]}]});
        let message = json!({"id":"a","type":"assistant","content":[{"type":"tool","id":"q","name":"question","state":{"status":"running","input":input}}]});
        let replay = updates(&message, Path::new("/")).unwrap();
        assert_eq!(replay[1]["status"], "failed");
        assert_eq!(replay[1]["rawInput"], input);
        assert_eq!(
            replay[2]["content"]["text"],
            "The previous question was interrupted when the session reopened. You can answer in the conversation:\nWhich city?"
        );
    }

    #[test]
    fn replays_full_tool_result_and_rich_content() {
        let state = json!({"status":"completed", "input":{"x":1},
            "content":[{"type":"image", "mimeType":"image/png", "data":"YWJj"}, {"type":"future", "value":4}],
            "structuredContent":{"ok":true}, "_meta":{"trace":"t"}, "futureField":7});
        let message = json!({"id":"a", "type":"assistant", "content":[{"type":"tool", "id":"t", "name":"custom", "state":state}]});
        let replay = updates(&message, Path::new("/")).unwrap();
        let result = &replay[1];
        assert_eq!(result["rawOutput"]["structuredContent"], json!({"ok":true}));
        assert_eq!(result["rawOutput"]["futureField"], 7);
        assert_eq!(result["rawOutput"]["content"], state["content"]);
        assert_eq!(result["content"][0]["content"], state["content"][0]);
    }

    #[test]
    fn replays_user_thought_and_tool_in_order() {
        let user = json!({"id":"u1","type":"user","text":"hello","files":[{"name":"note.md","mime":"text/markdown","source":{"uri":"file:///tmp/note.md"}}]});
        assert_eq!(
            updates(&user, Path::new("/tmp")).unwrap(),
            vec![
                json!({"sessionUpdate":"user_message_chunk","messageId":"u1","content":{"type":"text","text":"hello"}}),
                json!({"sessionUpdate":"user_message_chunk","messageId":"u1","content":{"type":"resource_link","uri":"file:///tmp/note.md","name":"note.md","mimeType":"text/markdown"}})
            ]
        );
        let assistant = json!({"id":"a1","type":"assistant","content":[{"type":"reasoning","text":"think"},{"type":"text","text":"answer"},{"type":"tool","id":"t1","name":"shell","state":{"status":"completed","input":{},"content":[{"type":"text","text":"done"}],"metadata":{}}}]});
        let actual = updates(&assistant, Path::new("/tmp")).unwrap();
        assert_eq!(
            actual
                .iter()
                .map(|value| value["sessionUpdate"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec![
                "agent_thought_chunk",
                "agent_message_chunk",
                "tool_call",
                "tool_call_update"
            ]
        );
        let compacted =
            json!({"id":"c1","type":"compaction","status":"completed","reason":"manual"});
        assert_eq!(
            updates(&compacted, Path::new("/tmp")).unwrap(),
            vec![
                json!({"sessionUpdate":"session_info_update","_meta":{"opencode/compaction":{"status":"completed","messageId":"c1","reason":"manual"}}})
            ]
        );
    }
}
