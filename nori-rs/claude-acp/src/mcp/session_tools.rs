//! The tools a session's Claude Code gets besides its own: the client's MCP servers, and Nori's
//! host tools when Nori runs this driver in its own process.
//!
//! Claude Code connects to MCP servers itself, so the client's servers are handed to it as they
//! are, whatever their transport. Nori's host tools are an in-process SDK server named
//! [`nori_host_tools::SERVER_NAME`]: the CLI asks for them over the stream-json pipe it already
//! has (`mcp_message`), and each call runs Nori's code directly. No socket, port or process.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;

use async_trait::async_trait;
use claude_code_agent_sdk::McpServerConfig;
use claude_code_agent_sdk::SdkMcpServer;
use claude_code_agent_sdk::ToolDefinition;
use claude_code_agent_sdk::types::mcp::McpHttpServerConfig;
use claude_code_agent_sdk::types::mcp::McpSdkServerConfig;
use claude_code_agent_sdk::types::mcp::McpSseServerConfig;
use claude_code_agent_sdk::types::mcp::McpStdioServerConfig;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::Meta;
use nori_host_tools::HostToolOutput;
use nori_host_tools::HostToolSpec;
use nori_host_tools::HostTools;
use serde_json::Value;
use serde_json::json;

/// The tools of one session, as the client asked for them.
#[derive(Clone, Default)]
pub struct SessionTools {
    /// The client's MCP servers.
    pub mcp_servers: Vec<McpServer>,
    /// Nori's own tools, when the session request carried a host-tools token (or, for a
    /// session prepared ahead of time, when it will be claimed by one that does).
    pub host_tools: Option<Arc<HostToolsBinding>>,
}

impl std::fmt::Debug for SessionTools {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionTools")
            .field("mcp_servers", &self.mcp_servers.len())
            .field("host_tools", &self.host_tools.is_some())
            .finish()
    }
}

impl SessionTools {
    /// The tools a session request names: its MCP servers, and host tools if its `_meta` carries
    /// a token registered in this process.
    pub fn from_request(mcp_servers: Vec<McpServer>, meta: Option<&Meta>) -> Self {
        Self {
            mcp_servers,
            host_tools: nori_host_tools::from_session_meta(meta).map(HostToolsBinding::bound),
        }
    }

    /// Every server to put in the CLI's `--mcp-config`, by name. A client server named like the
    /// built-in ones does not replace them.
    pub fn cli_servers(&self, reserved: &[&str]) -> HashMap<String, McpServerConfig> {
        let mut servers = HashMap::new();
        for server in &self.mcp_servers {
            let (name, config) = match server {
                McpServer::Stdio(stdio) => (
                    stdio.name.clone(),
                    McpServerConfig::Stdio(McpStdioServerConfig {
                        command: stdio.command.to_string_lossy().into_owned(),
                        args: (!stdio.args.is_empty()).then(|| stdio.args.clone()),
                        env: (!stdio.env.is_empty()).then(|| {
                            stdio
                                .env
                                .iter()
                                .map(|var| (var.name.clone(), var.value.clone()))
                                .collect()
                        }),
                    }),
                ),
                McpServer::Http(http) => (
                    http.name.clone(),
                    McpServerConfig::Http(McpHttpServerConfig {
                        url: http.url.clone(),
                        headers: (!http.headers.is_empty()).then(|| {
                            http.headers
                                .iter()
                                .map(|header| (header.name.clone(), header.value.clone()))
                                .collect()
                        }),
                    }),
                ),
                McpServer::Sse(sse) => (
                    sse.name.clone(),
                    McpServerConfig::Sse(McpSseServerConfig {
                        url: sse.url.clone(),
                        headers: (!sse.headers.is_empty()).then(|| {
                            sse.headers
                                .iter()
                                .map(|header| (header.name.clone(), header.value.clone()))
                                .collect()
                        }),
                    }),
                ),
                _ => {
                    tracing::warn!(
                        "Skipping an MCP server of a transport this driver does not know"
                    );
                    continue;
                }
            };
            if reserved.contains(&name.as_str()) || name == nori_host_tools::SERVER_NAME {
                tracing::warn!(server = %name, "Skipping an MCP server named like a built-in one");
                continue;
            }
            servers.insert(name, config);
        }
        if let Some(tools) = &self.host_tools {
            servers.insert(
                nori_host_tools::SERVER_NAME.to_owned(),
                McpServerConfig::Sdk(McpSdkServerConfig {
                    name: nori_host_tools::SERVER_NAME.to_owned(),
                    instance: Arc::new(HostToolsServer {
                        tools: Arc::clone(tools),
                    }),
                }),
            );
        }
        servers
    }
}

/// The host tool a Claude Code tool name calls, if it is one.
pub fn host_tool_name(tool_name: &str) -> Option<&str> {
    tool_name
        .strip_prefix("mcp__")?
        .strip_prefix(nori_host_tools::SERVER_NAME)?
        .strip_prefix("__")
}

/// Nori's tools for one session. Bound when the session is created, or, for a session prepared
/// ahead of time, when it is claimed; until then it describes the tools from the catalog Nori
/// published, which is what every session's tools look like.
#[derive(Default)]
pub struct HostToolsBinding {
    tools: OnceLock<Arc<dyn HostTools>>,
    active: Arc<Mutex<ActiveHostCalls>>,
}

#[derive(Default)]
struct ActiveHostCalls {
    cancelled: bool,
    calls: HashMap<String, tokio_util::sync::CancellationToken>,
    early_cancellations: std::collections::VecDeque<String>,
}

struct ActiveHostCall {
    active: Arc<Mutex<ActiveHostCalls>>,
    key: String,
}

impl Drop for ActiveHostCall {
    fn drop(&mut self) {
        self.active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .calls
            .remove(&self.key);
    }
}

impl std::fmt::Debug for HostToolsBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostToolsBinding")
            .field("bound", &self.tools.get().is_some())
            .finish()
    }
}

impl HostToolsBinding {
    pub fn bound(tools: Arc<dyn HostTools>) -> Arc<Self> {
        let binding = Self::default();
        drop(binding.tools.set(tools));
        Arc::new(binding)
    }

    /// The tools, once bound.
    pub fn tools(&self) -> Option<Arc<dyn HostTools>> {
        self.tools.get().cloned()
    }

    /// A prewarmed CLI has already read the global catalog. Reuse it only
    /// when the bound session advertises exactly the same tools and instructions.
    pub fn matches_prewarm_catalog(&self) -> bool {
        self.matches_catalog(nori_host_tools::catalog().as_deref())
    }

    fn matches_catalog(&self, catalog: Option<&nori_host_tools::Catalog>) -> bool {
        match (self.tools.get(), catalog) {
            (Some(tools), Some(catalog)) => {
                tools.specs() == catalog.specs && tools.instructions() == catalog.instructions
            }
            (None, _) => true,
            (Some(_), None) => false,
        }
    }

    /// Give a session prepared ahead of time its tools. The first binding stays.
    pub fn bind(&self, tools: Arc<dyn HostTools>) {
        drop(self.tools.set(tools));
    }

    fn specs(&self) -> Vec<HostToolSpec> {
        match self.tools.get() {
            Some(tools) => tools.specs(),
            None => nori_host_tools::catalog()
                .map(|catalog| catalog.specs.clone())
                .unwrap_or_default(),
        }
    }

    fn instructions(&self) -> String {
        match self.tools.get() {
            Some(tools) => tools.instructions(),
            None => nori_host_tools::catalog()
                .map(|catalog| catalog.instructions.clone())
                .unwrap_or_default(),
        }
    }

    async fn call(&self, name: &str, arguments: Value) -> HostToolOutput {
        match self.tools.get() {
            Some(tools) => tools.call(name, arguments).await,
            None => HostToolOutput::error("Nori's tools are not connected to this session yet"),
        }
    }

    async fn call_tracked(&self, id: Value, name: &str, arguments: Value) -> HostToolOutput {
        let key = id.to_string();
        let call_id = id
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_string());
        let cancellation = tokio_util::sync::CancellationToken::new();
        {
            let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
            if active.cancelled || active.early_cancellations.iter().any(|id| id == &key) {
                cancellation.cancel();
                active.early_cancellations.retain(|id| id != &key);
            }
            active.calls.insert(key.clone(), cancellation.clone());
        }
        let _guard = ActiveHostCall {
            active: Arc::clone(&self.active),
            key,
        };
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => HostToolOutput::cancelled(&call_id, name),
            output = self.call(name, arguments) => output,
        }
    }

    fn cancel_call(&self, id: &Value) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        let key = id.to_string();
        if let Some(token) = active.calls.get(&key) {
            token.cancel();
        } else if !active.early_cancellations.contains(&key) {
            if active.early_cancellations.len() >= 128 {
                active.early_cancellations.pop_front();
            }
            active.early_cancellations.push_back(key);
        }
    }

    /// Cancel all in-flight calls owned by this session.
    pub fn cancel_all(&self) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        active.cancelled = true;
        for token in active.calls.values() {
            token.cancel();
        }
    }

    /// Permit calls for the next prompt after session cancellation.
    pub fn reset_cancelled(&self) {
        let mut active = self.active.lock().unwrap_or_else(PoisonError::into_inner);
        active.cancelled = false;
        active.early_cancellations.clear();
    }
}

/// Nori's host tools as an in-process MCP server.
struct HostToolsServer {
    tools: Arc<HostToolsBinding>,
}

#[async_trait]
impl SdkMcpServer for HostToolsServer {
    async fn handle_message(&self, message: Value) -> claude_code_agent_sdk::errors::Result<Value> {
        let method = message["method"].as_str().unwrap_or_default();
        match method {
            "initialize" => Ok(json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": nori_host_tools::SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "instructions": self.tools.instructions(),
            })),
            "tools/list" => Ok(json!({
                "tools": self
                    .tools
                    .specs()
                    .into_iter()
                    .map(|spec| {
                        let mut tool = json!({
                            "name": spec.name,
                            "description": spec.description,
                            "inputSchema": spec.input_schema,
                        });
                        if let Some(annotations) = spec.annotations {
                            tool["annotations"] = annotations;
                        }
                        if let Some(meta) = spec.meta {
                            tool["_meta"] = Value::Object(meta);
                        }
                        tool
                    })
                    .collect::<Vec<_>>(),
            })),
            "tools/call" => {
                let params = &message["params"];
                let name = params["name"].as_str().unwrap_or_default();
                let id = message
                    .get("id")
                    .cloned()
                    .unwrap_or_else(|| json!(uuid::Uuid::new_v4().to_string()));
                let output = self
                    .tools
                    .call_tracked(id, name, params["arguments"].clone())
                    .await;
                Ok(output.as_mcp_result())
            }
            "notifications/cancelled" => {
                if let Some(id) = message
                    .get("params")
                    .and_then(|params| params.get("requestId"))
                {
                    self.tools.cancel_call(id);
                }
                Ok(json!({}))
            }
            method if method.starts_with("notifications/") => Ok(json!({})),
            method => Err(claude_code_agent_sdk::errors::McpError::method_not_found(method).into()),
        }
    }

    fn list_tools(&self) -> Vec<ToolDefinition> {
        self.tools
            .specs()
            .into_iter()
            .map(|spec| ToolDefinition {
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
                annotations: spec
                    .annotations
                    .and_then(|annotations| serde_json::from_value(annotations).ok()),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use nori_acp_kit::schema::EnvVariable;
    use nori_acp_kit::schema::HttpHeader;
    use nori_acp_kit::schema::McpServerHttp;
    use nori_acp_kit::schema::McpServerStdio;
    use nori_host_tools::HostToolFuture;
    use pretty_assertions::assert_eq;

    use super::*;

    struct Tools;

    impl HostTools for Tools {
        fn instructions(&self) -> String {
            "Use git_status.".to_owned()
        }

        fn specs(&self) -> Vec<HostToolSpec> {
            vec![HostToolSpec {
                name: "git_status".to_owned(),
                description: "What changed.".to_owned(),
                input_schema: json!({"type": "object", "properties": {}}),
                annotations: Some(json!({"readOnlyHint": true})),
                meta: None,
            }]
        }

        fn call(&self, name: &str, arguments: Value) -> HostToolFuture {
            let text = format!("{name}:{arguments}");
            let output = if name == "git_status" {
                HostToolOutput::text(text)
            } else {
                HostToolOutput::error(text)
            };
            Box::pin(async move { output })
        }
    }

    fn server() -> HostToolsServer {
        HostToolsServer {
            tools: HostToolsBinding::bound(Arc::new(Tools)),
        }
    }

    #[tokio::test]
    async fn the_server_lists_and_calls_host_tools() {
        let server = server();
        let init = server
            .handle_message(json!({"method": "initialize"}))
            .await
            .expect("initialize");
        assert_eq!(init["serverInfo"]["name"], "nori");
        assert_eq!(init["instructions"], "Use git_status.");

        let list = server
            .handle_message(json!({"method": "tools/list"}))
            .await
            .expect("list");
        assert_eq!(
            list,
            json!({"tools": [{
                "name": "git_status",
                "description": "What changed.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": {"readOnlyHint": true},
            }]})
        );

        let call = server
            .handle_message(json!({
                "method": "tools/call",
                "params": {"name": "git_status", "arguments": {"a": 1}},
            }))
            .await
            .expect("call");
        assert_eq!(
            call,
            json!({"content": [{"type": "text", "text": "git_status:{\"a\":1}"}], "isError": false})
        );
        assert!(
            server
                .handle_message(json!({"method": "resources/list"}))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn customized_catalog_is_not_reused_from_a_prepared_session() {
        let catalog = nori_host_tools::Catalog {
            specs: Tools.specs(),
            instructions: Tools.instructions(),
        };
        let default = HostToolsBinding::bound(Arc::new(Tools));
        assert!(default.matches_catalog(Some(&catalog)));
        let selection = nori_host_tools::ToolSelection {
            enabled: Some(Default::default()),
            instructions: vec!["Only the selected tools are available".to_owned()],
            ..Default::default()
        };
        let selected = nori_host_tools::SelectedTools::new(Arc::new(Tools), &selection);
        let bound = HostToolsBinding::bound(Arc::new(selected));
        assert!(!bound.matches_catalog(Some(&catalog)));
        let server = HostToolsServer { tools: bound };
        let init = server
            .handle_message(json!({"method": "initialize"}))
            .await
            .unwrap();
        assert_eq!(
            init["instructions"],
            "Only the selected tools are available"
        );
        let list = server
            .handle_message(json!({"method": "tools/list"}))
            .await
            .unwrap();
        assert_eq!(list["tools"], json!([]));
        let call = server
            .handle_message(
                json!({"method": "tools/call", "params": {"name": "git_status", "arguments": {}}}),
            )
            .await
            .unwrap();
        assert_eq!(call["isError"], true);
    }

    struct BlockingTools(Arc<tokio::sync::Notify>);

    impl HostTools for BlockingTools {
        fn instructions(&self) -> String {
            String::new()
        }
        fn specs(&self) -> Vec<HostToolSpec> {
            Vec::new()
        }
        fn call(&self, _name: &str, _arguments: Value) -> HostToolFuture {
            let started = Arc::clone(&self.0);
            Box::pin(async move {
                started.notify_one();
                std::future::pending::<HostToolOutput>().await
            })
        }
    }

    #[tokio::test]
    async fn cancellation_notification_and_session_cancel_return_correlated_receipts() {
        let started = Arc::new(tokio::sync::Notify::new());
        let binding = HostToolsBinding::bound(Arc::new(BlockingTools(Arc::clone(&started))));
        let server = Arc::new(HostToolsServer {
            tools: Arc::clone(&binding),
        });
        let task = {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                server.handle_message(json!({
                "id": 42, "method": "tools/call", "params": {"name": "slow", "arguments": {}}
            })).await.unwrap()
            })
        };
        started.notified().await;
        server
            .handle_message(
                json!({"method": "notifications/cancelled", "params": {"requestId": 42}}),
            )
            .await
            .unwrap();
        let result = task.await.unwrap();
        assert_eq!(
            result["structuredContent"],
            json!({"status":"cancelled", "call_id":"42", "tool":"slow"})
        );
        assert_eq!(result["isError"], true);
        assert!(binding.active.lock().unwrap().calls.is_empty());

        binding.cancel_all();
        let result = server
            .handle_message(json!({"id":"next", "method":"tools/call", "params":{"name":"slow"}}))
            .await
            .unwrap();
        assert_eq!(result["structuredContent"]["call_id"], "next");
        binding.reset_cancelled();
        assert!(!binding.active.lock().unwrap().cancelled);
        server
            .handle_message(
                json!({"method":"notifications/cancelled", "params":{"requestId":"early"}}),
            )
            .await
            .unwrap();
        let early = server
            .handle_message(json!({"id":"early", "method":"tools/call", "params":{"name":"slow"}}))
            .await
            .unwrap();
        assert_eq!(early["structuredContent"]["call_id"], "early");
    }

    struct RichTools(Value);
    impl HostTools for RichTools {
        fn instructions(&self) -> String {
            String::new()
        }
        fn specs(&self) -> Vec<HostToolSpec> {
            Vec::new()
        }
        fn call(&self, _name: &str, _arguments: Value) -> HostToolFuture {
            let result = self.0.clone();
            Box::pin(async move { HostToolOutput::from_mcp_result(result) })
        }
    }

    #[tokio::test]
    async fn claude_server_preserves_rich_mcp_results() {
        let result = json!({"content":[
            {"type":"image","mimeType":"image/png","data":"YWJj"},
            {"type":"resource","resource":{"uri":"file:///example","text":"hello"}}
        ],"structuredContent":{"ok":true},"_meta":{"key":"value"},"isError":false});
        let server = HostToolsServer {
            tools: HostToolsBinding::bound(Arc::new(RichTools(result.clone()))),
        };
        assert_eq!(
            server
                .handle_message(
                    json!({"id":"call", "method":"tools/call", "params":{"name":"rich"}})
                )
                .await
                .unwrap(),
            result
        );
    }

    #[test]
    fn client_servers_go_to_the_cli_and_host_tools_join_them() {
        let tools = SessionTools {
            mcp_servers: vec![
                McpServer::Stdio(
                    McpServerStdio::new("files", "/bin/files")
                        .args(vec!["--root".to_owned()])
                        .env(vec![EnvVariable::new("A", "1")]),
                ),
                McpServer::Http(
                    McpServerHttp::new("web", "http://127.0.0.1:9/mcp")
                        .headers(vec![HttpHeader::new("Authorization", "Bearer x")]),
                ),
                McpServer::Http(McpServerHttp::new("acp", "http://shadow")),
            ],
            host_tools: Some(HostToolsBinding::bound(Arc::new(Tools))),
        };
        let servers = tools.cli_servers(&["acp"]);
        let mut names: Vec<&str> = servers.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["files", "nori", "web"]);
        let McpServerConfig::Stdio(files) = &servers["files"] else {
            panic!("stdio");
        };
        assert_eq!(
            (
                files.command.as_str(),
                files.args.clone(),
                files.env.clone()
            ),
            (
                "/bin/files",
                Some(vec!["--root".to_owned()]),
                Some(HashMap::from([("A".to_owned(), "1".to_owned())]))
            )
        );
        let McpServerConfig::Http(web) = &servers["web"] else {
            panic!("http");
        };
        assert_eq!(
            web.headers.clone(),
            Some(HashMap::from([(
                "Authorization".to_owned(),
                "Bearer x".to_owned()
            )]))
        );
    }

    #[tokio::test]
    async fn an_unbound_session_lists_the_catalog_until_it_is_claimed() {
        let binding = Arc::new(HostToolsBinding::default());
        let output = binding.call("git_status", Value::Null).await;
        assert!(output.is_error);
        binding.bind(Arc::new(Tools));
        assert_eq!(binding.specs()[0].name, "git_status");
        let output = binding.call("git_status", json!({})).await;
        assert_eq!(output.text, "git_status:{}");
    }
}
