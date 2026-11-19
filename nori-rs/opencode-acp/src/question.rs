//! Bridge OpenCode question forms to the ACP host's user-input request.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use acp::UntypedMessage;
use nori_acp_kit::Cx;
use nori_acp_kit::acp;
use reqwest::Method;
use serde_json::Value;
use serde_json::json;

use crate::agent::Session;

fn request(form: &Value) -> Option<Value> {
    let session = form["sessionID"].as_str()?;
    let id = form["id"].as_str()?;
    let fields = form["fields"].as_array()?;
    if fields.is_empty() {
        return None;
    }
    let questions: Option<Vec<_>> = fields.iter().map(|field| {
        let key = field["key"].as_str()?;
        let kind = field["type"].as_str()?;
        if !matches!(kind, "string" | "multiselect") { return None; }
        if let Some(options) = field["options"].as_array() {
            let mut labels = std::collections::HashSet::new();
            if options.iter().any(|option| option["label"].as_str().is_none_or(|label| !labels.insert(label))) {
                return None;
            }
        }
        let options: Option<Vec<_>> = field["options"].as_array().map(|options| options.iter().map(|option| {
            json!({"label":option["label"],"description":option["description"].as_str().unwrap_or("")})
        }).collect());
        Some(json!({"id":key,"header":field["title"].as_str().unwrap_or("Question"),"question":field["description"].as_str().unwrap_or(""),"isOther":field["custom"].as_bool().unwrap_or(true),"isSecret":false,"options":options,"multiple":kind == "multiselect"}))
    }).collect();
    Some(
        json!({"sessionId":session,"callId":form["metadata"]["tool"]["id"].as_str().unwrap_or(id),"turnId":id,"questions":questions?}),
    )
}

fn answer(form: &Value, response: &Value) -> Option<Value> {
    if response["cancelled"] == true {
        return None;
    }
    let mut answer = serde_json::Map::new();
    for field in form["fields"].as_array()? {
        let key = field["key"].as_str()?;
        let answers = response["answers"][key]["answers"].as_array()?;
        if answers.is_empty() || answers.iter().any(|value| !value.is_string()) {
            return None;
        }
        let values: Vec<Value> = answers
            .iter()
            .map(|answer| {
                let label = answer.as_str().unwrap_or_default();
                let value = field["options"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|option| option["label"] == label)
                    .and_then(|option| option["value"].as_str())
                    .unwrap_or(label);
                json!(value)
            })
            .collect();
        let value = if field["type"] == "multiselect" {
            Value::Array(values)
        } else if answers.len() == 1 {
            values[0].clone()
        } else {
            return None;
        };
        answer.insert(key.to_owned(), value);
    }
    Some(json!({"answer":answer}))
}

pub(crate) async fn ask(session: Arc<Session>, client: Cx, form: Value) {
    let (Some(session_id), Some(form_id)) = (form["sessionID"].as_str(), form["id"].as_str())
    else {
        return;
    };
    let mut form_cancelled = false;
    let result = match request(&form) {
        Some(params) => match UntypedMessage::new("_nori/request_user_input", params) {
            Ok(request) => {
                let pending = client.send_request(request).block_task();
                tokio::pin!(pending);
                loop {
                    tokio::select! {
                        result = &mut pending => break result.ok(),
                        _ = tokio::time::sleep(std::time::Duration::from_millis(500)) => {
                            if session.cancelled.load(Ordering::SeqCst) || session.closed.load(Ordering::SeqCst) {
                                break None;
                            }
                            if let Ok(state) = session.server.request(Method::GET, &["session", session_id, "form", form_id], None).await
                                && state["data"]["state"]["status"] == "cancelled" {
                                    form_cancelled = true;
                                    break None;
                                }
                        }
                    }
                }
            }
            Err(_) => None,
        },
        None => None,
    };
    let cancelled = form_cancelled
        || session.cancelled.load(Ordering::SeqCst)
        || session.closed.load(Ordering::SeqCst)
        || result
            .as_ref()
            .is_some_and(|value| value["cancelled"] == true);
    if cancelled && result.is_none() {
        let call_id = form["metadata"]["tool"]["id"].as_str().unwrap_or(form_id);
        if let Ok(notification) = UntypedMessage::new(
            "_nori/user_input_cancelled",
            json!({"sessionId":session_id,"callId":call_id,"turnId":form_id}),
        ) {
            let _ = client.send_notification(notification);
        }
    }
    let body = if cancelled {
        None
    } else {
        result.as_ref().and_then(|response| answer(&form, response))
    };
    let submitted = if let Some(body) = body {
        match session
            .server
            .request(
                Method::POST,
                &["session", session_id, "form", form_id, "reply"],
                Some(&body),
            )
            .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(%error, "failed to submit OpenCode question answer");
                false
            }
        }
    } else {
        false
    };
    if !submitted {
        let _ = session
            .server
            .request(
                Method::DELETE,
                &["session", session_id, "form", form_id],
                None,
            )
            .await;
        if !cancelled {
            if let Ok(event) = UntypedMessage::new(
                "session/update",
                json!({"sessionId":session_id,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"The user question could not be completed; the turn was stopped."}}}),
            ) {
                let _ = client.send_notification(event);
            }
            let _ = session
                .server
                .request(
                    Method::POST,
                    &["session", session_id, "interrupt"],
                    Some(&json!({})),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::answer;
    use super::request;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn maps_child_question_and_answers() {
        let form = json!({"id":"frm_1","sessionID":"child","metadata":{"kind":"question","tool":{"id":"call_1"}},"fields":[{"key":"q0","type":"string","title":"Location","description":"Which city?","custom":true,"options":[{"label":"Paris","value":"paris-fr","description":"France"}]},{"key":"q1","type":"multiselect","title":"More","description":"Choose","options":[]}]});
        assert_eq!(
            request(&form).unwrap(),
            json!({"sessionId":"child","callId":"call_1","turnId":"frm_1","questions":[{"id":"q0","header":"Location","question":"Which city?","isOther":true,"isSecret":false,"options":[{"label":"Paris","description":"France"}],"multiple":false},{"id":"q1","header":"More","question":"Choose","isOther":true,"isSecret":false,"options":[],"multiple":true}]})
        );
        assert_eq!(
            answer(
                &form,
                &json!({"answers":{"q0":{"answers":["Paris"]},"q1":{"answers":["one","two"]}}})
            ),
            Some(json!({"answer":{"q0":"paris-fr","q1":["one","two"]}}))
        );
        assert_eq!(answer(&form, &json!({"cancelled":true})), None);
    }
}
