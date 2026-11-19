//! Translate ACP's MCP server configuration to OpenCode v2 configuration.

use acp::schema::v1::McpServer;
use nori_acp_kit::acp;
use reqwest::Method;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;
use std::hash::Hash;
use std::hash::Hasher;

use crate::OpenCodeServer;

pub(crate) async fn register(
    server: &OpenCodeServer,
    session_id: &str,
    cwd: &std::path::Path,
    servers: Vec<McpServer>,
) -> acp::Result<Vec<String>> {
    let directory = cwd.to_str().ok_or_else(acp::Error::invalid_params)?;
    let namespace = session_namespace(session_id);
    let mut registered = Vec::new();
    for definition in servers {
        let (name, config) = match definition {
            McpServer::Stdio(definition) => {
                let mut command = vec![
                    definition
                        .command
                        .to_str()
                        .ok_or_else(acp::Error::invalid_params)?
                        .to_owned(),
                ];
                command.extend(definition.args);
                let environment: Map<String, Value> = definition
                    .env
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (
                    definition.name,
                    json!({"type":"local","command":command,"environment":environment}),
                )
            }
            McpServer::Http(definition) => {
                let headers: Map<String, Value> = definition
                    .headers
                    .into_iter()
                    .map(|entry| (entry.name, entry.value.into()))
                    .collect();
                (
                    definition.name,
                    json!({"type":"remote","url":definition.url,"headers":headers,"oauth":false}),
                )
            }
            _ => {
                return Err(acp::Error::invalid_params()
                    .data("OpenCode supports stdio and HTTP MCP servers"));
            }
        };
        let scoped_name = scoped_name(&namespace, &name);
        registered.push(scoped_name.clone());
        server
            .request_with_query(
                Method::PUT,
                &["experimental", "mcp", &scoped_name],
                &[("location[directory]", directory)],
                Some(&json!({"config":config})),
            )
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }
    if !namespace.is_empty() {
        let session = server
            .request(Method::GET, &["session", session_id], None)
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let mut permissions = session["data"]["permissions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|rule| {
                !rule["action"].as_str().is_some_and(|action| {
                    action == "nori-acp-*" || action == format!("nori-acp-{namespace}-*_*")
                })
            })
            .collect::<Vec<_>>();
        permissions.push(json!({"action":"nori-acp-*","resource":"*","effect":"deny"}));
        permissions.push(
            json!({"action":format!("nori-acp-{namespace}-*_*"),"resource":"*","effect":"allow"}),
        );
        server
            .request(
                Method::PATCH,
                &["session", session_id],
                Some(&json!({"permissions":permissions})),
            )
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
    }
    Ok(registered)
}

fn session_namespace(session_id: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session_id.hash(&mut hasher);
    format!("{:012x}", hasher.finish() & 0xffff_ffff_ffff)
}

fn scoped_name(namespace: &str, original: &str) -> String {
    let sanitized = original
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        .take(11)
        .collect::<String>();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    original.hash(&mut hasher);
    let server_hash = hasher.finish() & 0xff_ffff;
    format!("nori-acp-{namespace}-{sanitized}-{server_hash:06x}")
}

#[cfg(test)]
mod tests {
    use super::scoped_name;
    use super::session_namespace;

    #[test]
    fn session_mcp_names_are_bounded_and_distinct() {
        let first = session_namespace("session-000000000000000000000000");
        let second = session_namespace("session-000000000000000000000001");
        let first_name = scoped_name(&first, "tool server/with spaces and a very long suffix");
        let second_name = scoped_name(&second, "tool server/with spaces and a very long suffix");
        assert_ne!(first_name, second_name);
        assert!(first_name.len() <= 64);
        assert!(first_name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
        }));
    }
}
