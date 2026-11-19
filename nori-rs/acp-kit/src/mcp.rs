//! The client's MCP servers, as plain data.

use crate::error::AgentError;
use crate::schema::McpServer;

/// One MCP server a client asked the agent to connect to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEndpoint {
    pub name: String,
    pub transport: McpTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<(String, String)>,
    },
    Http {
        url: String,
        headers: Vec<(String, String)>,
    },
    Sse {
        url: String,
        headers: Vec<(String, String)>,
    },
}

/// The servers of a `session/new`, with duplicate names and transports the kit does not know
/// refused rather than guessed at.
pub fn endpoints(servers: Vec<McpServer>) -> Result<Vec<McpEndpoint>, AgentError> {
    let mut endpoints: Vec<McpEndpoint> = Vec::with_capacity(servers.len());
    for server in servers {
        let (name, transport) = match server {
            McpServer::Stdio(server) => (
                server.name,
                McpTransport::Stdio {
                    command: server.command.to_string_lossy().into_owned(),
                    args: server.args,
                    env: server
                        .env
                        .into_iter()
                        .map(|entry| (entry.name, entry.value))
                        .collect(),
                },
            ),
            McpServer::Http(server) => (
                server.name,
                McpTransport::Http {
                    url: server.url,
                    headers: server
                        .headers
                        .into_iter()
                        .map(|entry| (entry.name, entry.value))
                        .collect(),
                },
            ),
            McpServer::Sse(server) => (
                server.name,
                McpTransport::Sse {
                    url: server.url,
                    headers: server
                        .headers
                        .into_iter()
                        .map(|entry| (entry.name, entry.value))
                        .collect(),
                },
            ),
            _ => return Err(AgentError::invalid("Unsupported MCP server transport")),
        };
        if endpoints.iter().any(|endpoint| endpoint.name == name) {
            return Err(AgentError::invalid(format!(
                "Duplicate MCP server name {name:?}"
            )));
        }
        endpoints.push(McpEndpoint { name, transport });
    }
    Ok(endpoints)
}
