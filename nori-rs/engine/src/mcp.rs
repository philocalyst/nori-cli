//! The MCP servers a client names in `session/new` (and load, resume, fork), as engine tools.
//!
//! Servers start in the background when the session opens, so opening never waits for them;
//! their tools join the model's list from the first step after they are ready. A server that
//! fails to start is logged and left out. Tools are named `<server>_<tool>`, as OpenCode names
//! them, and run with the session's permission rules like any other tool (action: that name).

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use async_trait::async_trait;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::ToolKind;
use nori_llm::ToolResultPart;
use nori_llm::ToolSpec;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParam;
use rmcp::service::Peer;
use rmcp::service::RoleClient;
use rmcp::service::RunningService;
use serde_json::Value;

use crate::tools::Check;
use crate::tools::Invocation;
use crate::tools::Tool;
use crate::tools::ToolContext;
use crate::tools::ToolOutput;

/// The session's MCP servers and the tools they offer.
#[derive(Default)]
pub(crate) struct McpTools {
    tools: Mutex<Vec<Arc<dyn Tool>>>,
    /// Kept so the servers stay connected for the session's life.
    services: Mutex<Vec<RunningService<RoleClient, ()>>>,
}

impl McpTools {
    /// Start `servers` in the background.
    pub(crate) fn start(servers: Vec<McpServer>, cwd: std::path::PathBuf) -> Arc<Self> {
        let tools = Arc::new(Self::default());
        if servers.is_empty() {
            return tools;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return tools;
        };
        for server in servers {
            let tools = Arc::clone(&tools);
            let cwd = cwd.clone();
            runtime.spawn(async move {
                match connect(&server, &cwd).await {
                    Ok((name, service, listed)) => {
                        let peer = service.peer().clone();
                        let added: Vec<Arc<dyn Tool>> = listed
                            .into_iter()
                            .map(|tool| {
                                Arc::new(McpTool::new(&name, tool, peer.clone())) as Arc<dyn Tool>
                            })
                            .collect();
                        tracing::info!(server = %name, tools = added.len(), "MCP server ready");
                        tools
                            .tools
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .extend(added);
                        tools
                            .services
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(service);
                    }
                    Err(error) => tracing::warn!(%error, "an MCP server did not start"),
                }
            });
        }
        tools
    }

    /// The tools ready now.
    pub(crate) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

async fn connect(
    server: &McpServer,
    cwd: &std::path::Path,
) -> Result<
    (
        String,
        RunningService<RoleClient, ()>,
        Vec<rmcp::model::Tool>,
    ),
    String,
> {
    let (name, service) = match server {
        McpServer::Stdio(stdio) => {
            let mut command = tokio::process::Command::new(&stdio.command);
            command.args(&stdio.args).current_dir(cwd).envs(
                stdio
                    .env
                    .iter()
                    .map(|variable| (&variable.name, &variable.value)),
            );
            let transport = rmcp::transport::TokioChildProcess::new(command)
                .map_err(|error| format!("{}: {error}", stdio.name))?;
            let service =
                ().serve(transport)
                    .await
                    .map_err(|error| format!("{}: {error}", stdio.name))?;
            (stdio.name.clone(), service)
        }
        McpServer::Http(http) => {
            let mut headers = reqwest::header::HeaderMap::new();
            for header in &http.headers {
                if let (Ok(name), Ok(value)) = (
                    reqwest::header::HeaderName::from_bytes(header.name.as_bytes()),
                    reqwest::header::HeaderValue::from_str(&header.value),
                ) {
                    headers.insert(name, value);
                }
            }
            let client = reqwest::Client::builder()
                .default_headers(headers)
                .build()
                .map_err(|error| format!("{}: {error}", http.name))?;
            let transport = rmcp::transport::StreamableHttpClientTransport::with_client(
                client,
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    http.url.clone(),
                ),
            );
            let service =
                ().serve(transport)
                    .await
                    .map_err(|error| format!("{}: {error}", http.name))?;
            (http.name.clone(), service)
        }
        other => return Err(format!("unsupported MCP transport: {other:?}")),
    };
    let tools = service
        .list_all_tools()
        .await
        .map_err(|error| format!("{name}: listing tools: {error}"))?;
    Ok((name, service, tools))
}

/// `name` with everything but letters, digits, `_` and `-` replaced, as providers want tool
/// names.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

struct McpTool {
    name: String,
    server: String,
    tool: rmcp::model::Tool,
    peer: Peer<RoleClient>,
}

impl McpTool {
    fn new(server: &str, tool: rmcp::model::Tool, peer: Peer<RoleClient>) -> Self {
        Self {
            name: sanitize(&format!("{server}_{}", tool.name)),
            server: server.to_owned(),
            tool,
            peer,
        }
    }
}

#[async_trait]
impl Tool for McpTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: self
                .tool
                .description
                .as_deref()
                .unwrap_or_default()
                .to_owned(),
            input_schema: Value::Object((*self.tool.input_schema).clone()),
        }
    }

    async fn prepare(&self, _input: &Value, _context: &ToolContext) -> Result<Invocation, String> {
        let title = self
            .tool
            .title
            .clone()
            .unwrap_or_else(|| self.tool.name.to_string());
        let mut invocation = Invocation::new(format!("{}: {title}", self.server), ToolKind::Other);
        invocation.checks = vec![Check::new(&self.name, "*")];
        Ok(invocation)
    }

    async fn run(&self, input: Value, _context: &ToolContext) -> ToolOutput {
        let arguments = match input {
            Value::Object(arguments) => Some(arguments),
            Value::Null => None,
            other => {
                return ToolOutput::error(format!("The arguments must be an object, not {other}"));
            }
        };
        let result = self
            .peer
            .call_tool(CallToolRequestParam {
                name: Cow::Owned(self.tool.name.to_string()),
                arguments,
            })
            .await;
        let result = match result {
            Ok(result) => result,
            Err(error) => return ToolOutput::error(format!("{}: {error}", self.server)),
        };
        let raw = serde_json::to_value(&result).unwrap_or(Value::Null);
        let mut parts = Vec::new();
        for block in raw["content"].as_array().into_iter().flatten() {
            match block["type"].as_str() {
                Some("text") => parts.push(ToolResultPart::Text {
                    text: block["text"].as_str().unwrap_or_default().to_owned(),
                }),
                Some("image") => {
                    if let (Some(media_type), Some(data)) =
                        (block["mimeType"].as_str(), block["data"].as_str())
                    {
                        parts.push(ToolResultPart::Image {
                            media_type: media_type.to_owned(),
                            data: data.to_owned(),
                        });
                    }
                }
                _ => parts.push(ToolResultPart::Text {
                    text: block.to_string(),
                }),
            }
        }
        if let Some(structured) = raw
            .get("structuredContent")
            .filter(|value| !value.is_null())
        {
            parts.push(ToolResultPart::Text {
                text: structured.to_string(),
            });
        }
        let (display, _) = nori_acp_kit::output_payload(
            &nori_host_tools::HostToolOutput::from_mcp_result(raw.clone()),
        );
        ToolOutput {
            result: parts,
            is_error: result.is_error.unwrap_or(false),
            display,
            raw_output: Some(raw),
            plan: None,
        }
    }
}
