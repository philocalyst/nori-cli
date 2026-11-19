//! Forward OpenCode permission requests and reject late replies after cancellation.

use crate::agent::Session;
use acp::schema::v1::PermissionOption;
use acp::schema::v1::PermissionOptionKind;
use acp::schema::v1::RequestPermissionOutcome;
use acp::schema::v1::RequestPermissionRequest;
use acp::schema::v1::ToolCallUpdate;
use acp::schema::v1::ToolCallUpdateFields;
use nori_acp_kit::Cx;
use nori_acp_kit::acp;
use reqwest::Method;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::Ordering;

pub(crate) async fn ask_permission(
    session: Arc<Session>,
    client: Cx,
    root: acp::schema::v1::SessionId,
    data: Value,
    child: Option<(String, String, i64, Option<String>)>,
) {
    let Some(id) = data["id"].as_str() else {
        return;
    };
    let Some(session_id) = data["sessionID"].as_str() else {
        return;
    };
    let tool_id = data["source"]["id"].as_str().unwrap_or(id);
    let title = data["action"].as_str().unwrap_or("OpenCode permission");
    let tool = ToolCallUpdate::new(
        tool_id.to_owned(),
        ToolCallUpdateFields::new().title(title.to_owned()),
    );
    let request_session = child.map_or(root, |(child, _, _, _)| child.into());
    let options = vec![
        PermissionOption::new("once", "Allow once", PermissionOptionKind::AllowOnce),
        PermissionOption::new("always", "Always allow", PermissionOptionKind::AllowAlways),
        PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
    ];
    let result = client
        .send_request(RequestPermissionRequest::new(
            request_session,
            tool,
            options,
        ))
        .block_task()
        .await;
    let decision = match result {
        Ok(response) => match response.outcome {
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.to_string() == "once" =>
            {
                "once"
            }
            RequestPermissionOutcome::Selected(selected)
                if selected.option_id.to_string() == "always" =>
            {
                "always"
            }
            _ => "reject",
        },
        Err(_) => "reject",
    };
    let _permission_reply = session.permission_reply.lock().await;
    let decision = if session.cancelled.load(Ordering::SeqCst) {
        "reject"
    } else {
        decision
    };
    let _ = session
        .server
        .request(
            Method::POST,
            &["session", session_id, "permission", id, "reply"],
            Some(&json!({"decision":decision})),
        )
        .await;
}
