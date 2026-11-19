//! The Codex configuration of one session: the agent's base configuration, the session's working
//! directory, and the MCP servers the client asked for.

use std::path::Path;

use codex_config::DEFAULT_MCP_SERVER_ENVIRONMENT_ID;
use codex_config::McpServerConfig;
use codex_config::McpServerTransportConfig;
use codex_core::config::Config;
use codex_utils_path_uri::LegacyAppPathString;
use nori_acp_kit::AgentError;
use nori_acp_kit::schema::McpServer;
use nori_acp_kit::schema::McpServerHttp;
use nori_acp_kit::schema::McpServerStdio;

use super::CodexAgent;

impl CodexAgent {
    /// Build a session config from base config, working directory, and MCP servers.
    /// This is shared between `new_session` and `load_session`.
    pub(super) fn build_session_config(
        &self,
        cwd: &Path,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Config, AgentError> {
        let mut config = self.config.clone();
        config.cwd = cwd.try_into().map_err(AgentError::internal)?;
        let cwd = config.cwd.clone();

        // Propagate any client-provided MCP servers that codex-rs supports.
        let mut new_mcp_servers = config.mcp_servers.get().clone();
        for mcp_server in mcp_servers {
            match mcp_server {
                // Not supported in codex
                McpServer::Sse(..) => {}
                McpServer::Http(McpServerHttp {
                    name, url, headers, ..
                }) => {
                    // Codex does not allow whitespace in MCP server names; replace with underscores.
                    let name = name.replace(|c: char| c.is_whitespace(), "_");
                    new_mcp_servers.insert(
                        name,
                        McpServerConfig {
                            transport: McpServerTransportConfig::StreamableHttp {
                                url,
                                bearer_token_env_var: None,
                                http_headers: if headers.is_empty() {
                                    None
                                } else {
                                    Some(headers.into_iter().map(|h| (h.name, h.value)).collect())
                                },
                                env_http_headers: None,
                                http_headers_helper: None,
                            },
                            required: false,
                            auth: Default::default(),
                            startup_readiness: Default::default(),
                            tool_input_schema_max_bytes: None,
                            omit_tools_from: None,
                            enabled: true,
                            startup_timeout_sec: None,
                            tool_timeout_sec: None,
                            disabled_tools: None,
                            enabled_tools: None,
                            disabled_reason: None,
                            scopes: None,
                            oauth: None,
                            oauth_resource: None,
                            tools: Default::default(),
                            environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                            supports_parallel_tool_calls: false,
                            default_tools_approval_mode: None,
                        },
                    );
                }
                McpServer::Stdio(McpServerStdio {
                    name,
                    command,
                    args,
                    env,
                    ..
                }) => {
                    // Codex does not allow whitespace in MCP server names; replace with underscores.
                    let name = name.replace(|c: char| c.is_whitespace(), "_");
                    new_mcp_servers.insert(
                        name,
                        McpServerConfig {
                            transport: McpServerTransportConfig::Stdio {
                                command: command.display().to_string(),
                                args,
                                env: if env.is_empty() {
                                    None
                                } else {
                                    Some(env.into_iter().map(|env| (env.name, env.value)).collect())
                                },
                                env_vars: vec![],
                                cwd: Some(LegacyAppPathString::from_path(&cwd.to_path_buf())),
                            },
                            required: false,
                            auth: Default::default(),
                            startup_readiness: Default::default(),
                            tool_input_schema_max_bytes: None,
                            omit_tools_from: None,
                            enabled: true,
                            startup_timeout_sec: None,
                            tool_timeout_sec: None,
                            disabled_tools: None,
                            enabled_tools: None,
                            disabled_reason: None,
                            scopes: None,
                            oauth: None,
                            oauth_resource: None,
                            tools: Default::default(),
                            environment_id: DEFAULT_MCP_SERVER_ENVIRONMENT_ID.to_string(),
                            supports_parallel_tool_calls: false,
                            default_tools_approval_mode: None,
                        },
                    );
                }
                _ => {}
            }
        }

        config
            .mcp_servers
            .set(new_mcp_servers)
            .map_err(AgentError::internal)?;

        Ok(config)
    }
}
