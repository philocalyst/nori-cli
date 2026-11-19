use std::fs::File;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use tracing::warn;

use crate::registry::AcpAgentConfig;
use nori_config::AcpProxyConfig;

#[derive(Clone)]
pub(super) struct WireLogger {
    inner: Arc<Mutex<File>>,
    agent: String,
    child_pid: i64,
}

impl WireLogger {
    pub(super) fn new(
        config: &AcpProxyConfig,
        agent_config: &AcpAgentConfig,
        pid: u32,
    ) -> Result<Self> {
        std::fs::create_dir_all(&config.log_dir).with_context(|| {
            format!(
                "Failed to create ACP wire log directory {}",
                config.log_dir.display()
            )
        })?;

        let child_pid = i64::from(pid);
        let path = log_path(&config.log_dir, &agent_config.provider_slug, child_pid);
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open ACP wire log {}", path.display()))?;

        Ok(Self {
            inner: Arc::new(Mutex::new(file)),
            agent: agent_config.provider_slug.clone(),
            child_pid,
        })
    }

    pub(super) fn record(&self, direction: WireDirection, line: &str) {
        if let Err(error) = self.try_record(direction, line) {
            warn!("Failed to write ACP wire log entry: {error}");
        }
    }

    fn try_record(&self, direction: WireDirection, line: &str) -> Result<()> {
        let record = match serde_json::from_str::<Value>(line) {
            Ok(mut message) => {
                redact_wire_value(&mut message, false);
                json!({
                "ts_ms": unix_time_millis(),
                "direction": direction.as_str(),
                "agent": self.agent,
                "child_pid": self.child_pid,
                "message": message,
                })
            }
            Err(error) => json!({
                "ts_ms": unix_time_millis(),
                "direction": direction.as_str(),
                "agent": self.agent,
                "child_pid": self.child_pid,
                "raw_line": line,
                "parse_error": error.to_string(),
            }),
        };

        let mut file = self
            .inner
            .lock()
            .map_err(|error| anyhow::anyhow!("ACP wire log lock poisoned: {error}"))?;
        serde_json::to_writer(&mut *file, &record).context("Failed to serialize ACP wire log")?;
        file.write_all(b"\n")
            .context("Failed to append ACP wire log newline")?;
        Ok(())
    }
}

fn redact_wire_value(value: &mut Value, in_schema: bool) {
    match value {
        Value::Object(object) => {
            for (key, value) in object.iter_mut() {
                let child_in_schema = in_schema
                    || matches!(
                        key.to_ascii_lowercase().as_str(),
                        "inputschema" | "schema" | "properties" | "defs" | "$defs" | "items"
                    );
                if !in_schema && is_sensitive_key(key) {
                    *value = Value::String("<redacted>".to_owned());
                } else if key.eq_ignore_ascii_case("url") {
                    if let Value::String(url) = value {
                        *url = redact_url_query(url);
                    }
                    redact_wire_value(value, child_in_schema);
                } else if key.eq_ignore_ascii_case("headers") && value.is_array() {
                    redact_header_values(value);
                } else {
                    redact_wire_value(value, child_in_schema);
                }
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| redact_wire_value(value, in_schema)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn redact_header_values(value: &mut Value) {
    let Value::Array(headers) = value else {
        return;
    };
    for header in headers {
        let Value::Object(header) = header else {
            continue;
        };
        let sensitive = header
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(is_sensitive_key);
        if sensitive && header.contains_key("value") {
            header.insert("value".to_owned(), Value::String("<redacted>".to_owned()));
        }
    }
}

fn is_sensitive_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "api-key"
            | "apikey"
            | "access-token"
            | "access_token"
            | "refresh-token"
            | "refresh_token"
            | "id-token"
            | "id_token"
            | "password"
            | "client-secret"
            | "client_secret"
            | "secret"
    ) || {
        let lower = key.to_ascii_lowercase();
        lower.ends_with("_token") || lower.ends_with("-token")
    }
}

fn redact_url_query(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_owned();
    };
    let query = query
        .split('#')
        .next()
        .unwrap_or(query)
        .split('&')
        .map(|part| {
            let Some((key, _)) = part.split_once('=') else {
                return part.to_owned();
            };
            if is_sensitive_key(key) || key.eq_ignore_ascii_case("token") {
                format!("{key}=<redacted>")
            } else {
                part.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{query}")
}

#[derive(Clone, Copy)]
pub(super) enum WireDirection {
    ClientToAgent,
    AgentToClient,
}

impl WireDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::ClientToAgent => "client_to_agent",
            Self::AgentToClient => "agent_to_client",
        }
    }
}

fn log_path(log_dir: &Path, agent_slug: &str, child_pid: i64) -> std::path::PathBuf {
    let timestamp = unix_time_millis();
    let agent = sanitize_filename(agent_slug);
    log_dir.join(format!("{timestamp}-{child_pid}-{agent}.jsonl"))
}

fn sanitize_filename(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn unix_time_millis() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    i64::try_from(millis).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::redact_wire_value;
    use serde_json::json;

    #[test]
    fn redacts_auth_headers_and_credentialed_urls_but_keeps_content() {
        let mut value = json!({
            "method": "session/new",
            "params": {
                "mcpServers": [{
                    "url": "https://example.test/sse?project=demo&token=secret-token",
                    "headers": {
                        "Authorization": "Bearer secret",
                        "cOoKiE": "sid=private"
                    },
                    "description": "ordinary model text stays visible"
                }],
                "content": "do not redact this model text"
            }
        });

        redact_wire_value(&mut value, false);

        assert_eq!(
            value["params"]["mcpServers"][0]["headers"]["Authorization"],
            "<redacted>"
        );
        assert_eq!(
            value["params"]["mcpServers"][0]["headers"]["cOoKiE"],
            "<redacted>"
        );
        assert_eq!(
            value["params"]["mcpServers"][0]["url"],
            "https://example.test/sse?project=demo&token=<redacted>"
        );
        assert_eq!(value["params"]["content"], "do not redact this model text");
    }

    #[test]
    fn preserves_schema_password_property_and_redacts_acp_header_array() {
        let mcp_server = nori_protocol::acp::v1::McpServer::Http(
            nori_protocol::acp::v1::McpServerHttp::new(
                "private",
                "https://example.test/mcp?token=url-secret",
            )
            .headers(vec![nori_protocol::acp::v1::HttpHeader::new(
                "Authorization",
                "Bearer header-secret",
            )]),
        );
        let serialized = serde_json::to_value(&mcp_server).expect("ACP MCP server serializes");
        let mut value = json!({
            "params": {
                "mcpServers": [serialized, {
                    "inputSchema": {
                        "properties": {"password": {"type": "string"}},
                        "required": ["password"]
                    }
                }]
            }
        });

        redact_wire_value(&mut value, false);

        assert_eq!(
            value["params"]["mcpServers"][0]["headers"][0]["value"],
            "<redacted>"
        );
        assert_eq!(
            value["params"]["mcpServers"][0]["url"],
            "https://example.test/mcp?token=<redacted>"
        );
        assert_eq!(
            value["params"]["mcpServers"][1]["inputSchema"]["properties"]["password"]["type"],
            "string"
        );
    }
}
