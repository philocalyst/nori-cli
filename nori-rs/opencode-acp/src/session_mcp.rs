//! Session-owned OpenCode MCP registrations on the shared service.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use acp::schema::v1::McpServer;
use nori_acp_kit::acp;
use reqwest::Method;
use tokio::sync::Mutex;

use crate::OpenCodeServer;

pub(crate) type SharedSessionMcp = Arc<Mutex<SessionMcp>>;

pub(crate) struct SessionMcp {
    id: String,
    cwd: PathBuf,
    servers: Vec<McpServer>,
    names: Vec<String>,
    closed: bool,
}

impl SessionMcp {
    pub(crate) async fn create(
        server: &OpenCodeServer,
        id: String,
        cwd: PathBuf,
        servers: Vec<McpServer>,
    ) -> acp::Result<SharedSessionMcp> {
        let cwd = std::fs::canonicalize(cwd).map_err(acp::Error::into_internal_error)?;
        let names = crate::mcp::register(server, &id, &cwd, servers.clone()).await?;
        Ok(Arc::new(Mutex::new(Self {
            id,
            cwd,
            servers,
            names,
            closed: false,
        })))
    }

    pub(crate) async fn prepare(
        shared: &SharedSessionMcp,
        server: &OpenCodeServer,
    ) -> acp::Result<()> {
        let mut state = shared.lock().await;
        if state.closed {
            return Err(acp::Error::invalid_params().data("Session is closed"));
        }
        let directory = state.cwd.to_str().ok_or_else(acp::Error::invalid_params)?;
        let current = server
            .request_with_query(
                Method::GET,
                &["mcp"],
                &[("location[directory]", directory)],
                None,
            )
            .await
            .ok();
        let all_connected = state.names.is_empty()
            || current
                .as_ref()
                .and_then(|value| value["data"].as_array())
                .is_some_and(|entries| {
                    state.names.iter().all(|name| {
                        entries.iter().any(|entry| {
                            entry["name"].as_str() == Some(name.as_str())
                                && entry["status"]["status"].as_str() == Some("connected")
                        })
                    })
                });
        state.names =
            crate::mcp::register(server, &state.id, &state.cwd, state.servers.clone()).await?;
        // OpenCode 2.1 refreshes its tool registry asynchronously after a new connection.
        if !all_connected {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(())
    }

    pub(crate) async fn close(shared: &SharedSessionMcp, server: &OpenCodeServer) {
        let mut state = shared.lock().await;
        state.closed = true;
        let Some(directory) = state.cwd.to_str() else {
            return;
        };
        for name in &state.names {
            if let Err(error) = server
                .request_with_query(
                    Method::DELETE,
                    &["experimental", "mcp", name],
                    &[("location[directory]", directory)],
                    None,
                )
                .await
            {
                tracing::warn!(%error, "could not remove session MCP registration");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SessionMcp;
    use crate::OpenCodeServer;
    use acp::schema::v1::McpServer;
    use acp::schema::v1::McpServerHttp;
    use nori_acp_kit::acp;
    use pretty_assertions::assert_eq;
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::Mutex;

    #[tokio::test]
    async fn roots_keep_distinct_registrations_and_refresh_after_another_closes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = calls.clone();
        let responder = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buffer = [0; 8192];
                let Ok(count) = stream.read(&mut buffer).await else {
                    break;
                };
                let request = String::from_utf8_lossy(&buffer[..count]);
                let first = request.lines().next().unwrap_or_default().to_owned();
                observed.lock().await.push(first.clone());
                let body = if first.starts_with("GET /api/session/") {
                    r#"{"data":{"permissions":[]}}"#
                } else {
                    "{}"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                if stream.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
            }
        });
        let server = OpenCodeServer::connect(&format!("http://{address}"), "", Path::new("/"))
            .await
            .unwrap();
        let definition =
            McpServer::Http(McpServerHttp::new("nori-client", "http://127.0.0.1:12345"));
        let first = SessionMcp::create(
            &server,
            "first-root".into(),
            PathBuf::from("/tmp"),
            vec![definition.clone()],
        )
        .await
        .unwrap();
        let second = SessionMcp::create(
            &server,
            "second-root".into(),
            PathBuf::from("/tmp"),
            vec![definition],
        )
        .await
        .unwrap();
        SessionMcp::prepare(&first, &server).await.unwrap();
        SessionMcp::close(&first, &server).await;
        SessionMcp::prepare(&second, &server).await.unwrap();
        assert!(SessionMcp::prepare(&first, &server).await.is_err());
        let calls = calls.lock().await;
        let puts: Vec<_> = calls
            .iter()
            .filter(|line| line.starts_with("PUT /api/experimental/mcp/"))
            .collect();
        let deletes: Vec<_> = calls
            .iter()
            .filter(|line| line.starts_with("DELETE /api/experimental/mcp/"))
            .collect();
        assert_eq!(puts.len(), 4);
        assert_eq!(deletes.len(), 1);
        let path = |line: &str| line.split_whitespace().nth(1).unwrap().to_owned();
        assert_eq!(path(puts[0]), path(puts[2]));
        assert_eq!(path(puts[1]), path(puts[3]));
        assert_ne!(path(puts[0]), path(puts[1]));
        assert_eq!(path(deletes[0]), path(puts[0]));
        responder.abort();
    }
}
