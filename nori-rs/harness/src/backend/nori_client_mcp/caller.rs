//! Transport-owned caller identity for a provider's inherited Nori endpoint.
use super::*;

impl NoriClientService {
    pub(in crate::backend) async fn for_caller(
        &self,
        caller: Option<&str>,
        tool: &str,
    ) -> Result<Self, McpError> {
        let Some(caller) = caller else {
            return Ok(self.clone());
        };
        let orchestrator = self
            .shared
            .orchestrator
            .for_caller(caller)
            .await
            .map_err(|error| McpError::invalid_params(error, None))?;
        if orchestrator.is_native_caller()
            && matches!(
                tool,
                "conversation" | "create_goal" | "update_goal" | "get_goal"
            )
        {
            return Err(McpError::invalid_params(
                "This native child can delegate tasks and read its own conversation_context, but root conversation programs and goal tools are unavailable in its caller scope",
                None,
            ));
        }
        if orchestrator.is_native_caller() && tool.starts_with("monitor_") {
            return Err(McpError::invalid_params(
                "Monitors belong to Nori actors, and a provider's own child is not one: arm the monitor from the session that started it, or delegate with `delegate_task` to an actor that can",
                None,
            ));
        }
        let mut service = self.clone();
        if orchestrator.is_native_caller() {
            service.shared.caller_key = caller.to_owned();
        }
        service.shared.orchestrator = orchestrator;
        Ok(service)
    }
}

pub(super) fn session_from_meta(meta: &rmcp::model::Meta) -> Result<Option<&str>, McpError> {
    meta.0
        .get("ai.opencode/sessionID")
        .map(|value| {
            value
                .as_str()
                .filter(|session| !session.trim().is_empty())
                .ok_or_else(|| {
                    McpError::invalid_params("Invalid OpenCode caller session metadata", None)
                })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn real_mcp_transport_preserves_caller_metadata_and_rejects_foreign_sessions() {
        use rmcp::ServiceExt;
        let service = super::super::tests::service();
        service
            .shared
            .orchestrator
            .bind(crate::backend::orchestrator::Binding {
                session_id: "root".into(),
                approval_policy: tokio::sync::watch::channel(nori_config::AskForApproval::Never).1,
            });
        let server = NoriClientServer::spawn(service.shared.clone(), true)
            .await
            .unwrap();
        let transport = super::super::tests::authenticated_transport(
            server.url().to_string(),
            server.authorization_header_value.clone(),
        );
        let client = ().serve(transport).await.unwrap();
        for caller in ["root", "foreign"] {
            let request = serde_json::from_value::<rmcp::model::ClientRequest>(serde_json::json!({
                "method": "tools/call",
                "params": {"name": "task_list", "arguments": {}, "_meta": {"ai.opencode/sessionID": caller}},
            })).unwrap();
            let result = client.peer().send_request(request).await;
            assert_eq!(result.is_ok(), caller == "root");
        }
        let root_without_metadata = client
            .call_tool(rmcp::model::CallToolRequestParam {
                name: "task_list".into(),
                arguments: None,
            })
            .await
            .unwrap();
        assert_ne!(root_without_metadata.is_error, Some(true));
        let _ = client.cancel().await;
    }

    #[test]
    fn caller_identity_is_transport_metadata_and_malformed_metadata_fails_closed() {
        let mut meta = rmcp::model::Meta::default();
        assert_eq!(session_from_meta(&meta).unwrap(), None);
        meta.0
            .insert("ai.opencode/sessionID".into(), serde_json::json!("child"));
        assert_eq!(session_from_meta(&meta).unwrap(), Some("child"));
        meta.0.insert(
            "ai.opencode/sessionID".into(),
            serde_json::json!({"session":"child"}),
        );
        assert!(session_from_meta(&meta).is_err());
        meta.0
            .insert("ai.opencode/sessionID".into(), serde_json::json!(""));
        assert!(session_from_meta(&meta).is_err());
    }
}
