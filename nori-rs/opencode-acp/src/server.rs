use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures::Stream;
use futures::StreamExt;
use reqwest::Method;
use serde_json::Value;
use tokio::process::Child;

/// A failed call to the OpenCode server as the error the host sees: an unreachable server is
/// said to be unreachable, anything else keeps its message.
pub(crate) fn failure(error: Box<dyn std::error::Error + Send + Sync>) -> nori_acp_kit::AgentError {
    match error.downcast_ref::<reqwest::Error>() {
        Some(source) if source.is_connect() || source.is_timeout() => {
            nori_acp_kit::AgentError::Unreachable(error.to_string())
        }
        Some(_) | None => nori_acp_kit::AgentError::internal(error),
    }
}

/// An isolated OpenCode v2 server and its authenticated API client.
pub struct OpenCodeServer {
    child: Option<Child>,
    client: reqwest::Client,
    base_url: reqwest::Url,
    password: String,
}

impl OpenCodeServer {
    /// Connect to the already running OpenCode HTTP server.
    ///
    /// Nori deliberately does not spawn `opencode`: the server is owned by the
    /// user's OpenCode installation and can be shared by the CLI, desktop app,
    /// and other ACP clients. Set `OPENCODE_SERVER_URL` (or
    /// `NORI_OPENCODE_URL`) to override the standard loopback endpoint.
    pub async fn start(
        cwd: &Path,
        environment: &HashMap<OsString, OsString>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let url = environment
            .get(std::ffi::OsStr::new("NORI_OPENCODE_URL"))
            .or_else(|| environment.get(std::ffi::OsStr::new("OPENCODE_SERVER_URL")))
            .cloned()
            .or_else(|| std::env::var_os("NORI_OPENCODE_URL"))
            .or_else(|| std::env::var_os("OPENCODE_SERVER_URL"));
        let password = environment
            .get(std::ffi::OsStr::new("OPENCODE_PASSWORD"))
            .cloned()
            .or_else(|| std::env::var_os("OPENCODE_PASSWORD"));
        if let Some(url) = url {
            return Self::connect(
                url.to_string_lossy().as_ref(),
                password.unwrap_or_default().to_string_lossy().as_ref(),
                cwd,
            )
            .await;
        }
        if let Some(registration) = crate::service_discovery::read(environment) {
            let password = password.map_or(registration.password, |value| {
                value.to_string_lossy().into_owned()
            });
            let server = Self::connect(&registration.url, &password, cwd).await?;
            let info = tokio::time::timeout(
                Duration::from_secs(10),
                server.request(Method::GET, &["info"], None),
            )
            .await??;
            if info["pid"].as_i64() != Some(registration.pid)
                || registration
                    .version
                    .as_deref()
                    .is_some_and(|version| info["version"].as_str() != Some(version))
            {
                return Err(
                    "OpenCode service registration does not match its health response".into(),
                );
            }
            return Ok(server);
        }
        Self::connect(
            "http://127.0.0.1:4096",
            password.unwrap_or_default().to_string_lossy().as_ref(),
            cwd,
        )
        .await
    }

    /// Connect to an OpenCode v2 server without taking ownership of its process.
    pub async fn connect(
        address: &str,
        password: &str,
        _cwd: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut base_url: reqwest::Url = address.parse()?;
        if base_url.path().is_empty() || base_url.path() == "/" {
            base_url.set_path("/api/");
        } else if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }
        let server = Self {
            child: None,
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .build()?,
            base_url,
            password: password.to_owned(),
        };
        tokio::time::timeout(
            Duration::from_secs(10),
            server.request(Method::GET, &["info"], None),
        )
        .await??;
        Ok(server)
    }

    /// Send a v2 JSON API request. Each path element is escaped as one URL segment.
    pub async fn request(
        &self,
        method: Method,
        path: &[&str],
        body: Option<&Value>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.request_with_query(method, path, &[], body).await
    }

    /// OpenCode can expose its HTTP server before location plugins finish activating.
    /// A 503 during catalog discovery is temporary; other failures remain visible.
    pub(crate) async fn request_catalog(
        &self,
        path: &[&str],
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let result =
                tokio::time::timeout_at(deadline, self.request(Method::GET, path, None)).await?;
            match result {
                Err(error)
                    if error.downcast_ref::<reqwest::Error>().is_some_and(|error| {
                        error.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
                    }) && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep_until(
                        (tokio::time::Instant::now() + Duration::from_millis(200)).min(deadline),
                    )
                    .await;
                }
                result => return result,
            }
        }
    }

    /// Send a v2 JSON API request with query parameters.
    pub async fn request_with_query(
        &self,
        method: Method,
        path: &[&str],
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| "invalid server URL")?
            .pop_if_empty()
            .extend(path.iter().copied());
        let mut request = self
            .client
            .request(method, url)
            .query(query)
            .timeout(Duration::from_secs(30));
        if !self.password.is_empty() {
            request = request.basic_auth("opencode", Some(&self.password));
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await?.error_for_status()?;
        let bytes = response.bytes().await?;
        if bytes.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&bytes)?)
        }
    }

    /// Subscribe to the server event stream. Consumers must preserve event ordering.
    pub async fn events(
        &self,
    ) -> Result<
        impl Stream<
            Item = Result<
                eventsource_stream::Event,
                eventsource_stream::EventStreamError<reqwest::Error>,
            >,
        > + use<>,
        Box<dyn std::error::Error + Send + Sync>,
    > {
        let url = self.base_url.join("event")?;
        let mut request = self.client.get(url);
        if !self.password.is_empty() {
            request = request.basic_auth("opencode", Some(&self.password));
        }
        let response = request.send().await?.error_for_status()?;
        Ok(response.bytes_stream().eventsource().boxed())
    }

    /// Stop the server and wait until the process has exited.
    pub async fn shutdown(mut self) -> std::io::Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::path::Path;
    use std::time::Duration;

    use futures::StreamExt;
    use reqwest::Method;
    use serde_json::json;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    use super::OpenCodeServer;

    #[tokio::test]
    async fn discovers_and_authenticates_the_registered_service() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let count = stream.read(&mut buffer).await.unwrap();
                let request = String::from_utf8_lossy(&buffer[..count]).to_ascii_lowercase();
                assert!(request.contains("authorization: basic b3blbmnvzgu6dgvzda=="));
                let body = r#"{"pid":42,"version":"2.1.4"}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let state = tempfile::tempdir().unwrap();
        std::fs::create_dir(state.path().join("opencode")).unwrap();
        std::fs::write(state.path().join("opencode/service.json"), json!({"url": format!("http://{address}"), "pid": 42, "version":"2.1.4", "password":"test"}).to_string()).unwrap();
        let environment = HashMap::from([(
            OsString::from("XDG_STATE_HOME"),
            state.path().as_os_str().to_owned(),
        )]);
        let server = OpenCodeServer::start(Path::new("/"), &environment).await;
        assert!(server.is_ok());
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn catalog_retries_only_temporary_service_unavailable() {
        for (statuses, expected_requests, succeeds) in
            [(vec![503, 503, 200], 3, true), (vec![401], 1, false)]
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let responder = tokio::spawn(async move {
                let mut count = 0;
                for status in statuses {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut buffer = [0; 4096];
                    let _ = stream.read(&mut buffer).await.unwrap();
                    count += 1;
                    let body = if status == 200 { "{}" } else { "" };
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
                count
            });
            let child = tokio::process::Command::new("/bin/sleep")
                .arg("60")
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let server = OpenCodeServer {
                child: Some(child),
                client: reqwest::Client::builder().no_proxy().build().unwrap(),
                base_url: format!("http://{address}/api/").parse().unwrap(),
                password: "test".into(),
            };
            assert_eq!(server.request_catalog(&["model"]).await.is_ok(), succeeds);
            assert_eq!(responder.await.unwrap(), expected_requests);
        }
    }

    #[tokio::test]
    async fn real_server_session_and_event_stream() {
        if std::env::var_os("NORI_OPENCODE_URL").is_none()
            && std::env::var_os("OPENCODE_SERVER_URL").is_none()
        {
            return;
        }
        let Ok(output) = std::process::Command::new("which").arg("opencode").output() else {
            return;
        };
        if !output.status.success() {
            return;
        }
        let executable = String::from_utf8(output.stdout).expect("executable path is UTF-8");
        let Ok(version) = std::process::Command::new(executable.trim())
            .arg("--version")
            .output()
        else {
            return;
        };
        if !String::from_utf8_lossy(&version.stdout).contains("v2.") {
            return;
        }
        let home =
            std::env::temp_dir().join(format!("nori-opencode-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&home).expect("temporary home");
        let mut environment = HashMap::<OsString, OsString>::new();
        environment.insert("HOME".into(), home.as_os_str().into());
        environment.insert("OPENCODE_CONFIG_CONTENT".into(), "{}".into());
        for (name, path) in [
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
        ] {
            environment.insert(name.into(), home.join(path).into());
        }
        let server = OpenCodeServer::start(&home, &environment)
            .await
            .expect("start OpenCode v2");
        let config = server
            .request(Method::GET, &["config"], None)
            .await
            .expect("config");
        assert!(
            config
                .as_array()
                .expect("config entries")
                .iter()
                .any(|entry| {
                    entry["info"]["experimental"]["subagent_depth"]
                        == json!(9_007_199_254_740_991_i64)
                        && entry["info"]["permissions"][0]["effect"] == "allow"
                })
        );
        let mut events = server.events().await.expect("subscribe to events");
        let event = tokio::time::timeout(Duration::from_secs(5), events.next())
            .await
            .expect("event timeout")
            .expect("event stream closed")
            .expect("event parsing");
        let data: serde_json::Value = serde_json::from_str(&event.data).expect("event JSON");
        assert_eq!(data["type"], "server.connected");
        let session = server
            .request(Method::POST, &["session"], Some(&json!({})))
            .await
            .expect("create session");
        assert!(session["data"]["id"].as_str().is_some());
        let _ = crate::mcp::register(
            &server,
            "test-session",
            &home,
            vec![nori_acp_kit::schema::McpServer::Http(
                nori_acp_kit::schema::McpServerHttp::new("test-server", "http://127.0.0.1:1"),
            )],
        )
        .await
        .expect("register HTTP MCP server");
        drop(events);
        server.shutdown().await.expect("stop server");
        environment.insert("OPENCODE_CONFIG_CONTENT".into(), json!({"experimental":{"subagent_depth":1},"agents":{"general":{"permissions":[{"action":"subagent","resource":"*","effect":"deny"}]}}}).to_string().into());
        let server = OpenCodeServer::start(&home, &environment)
            .await
            .expect("start OpenCode with explicit restrictions");
        let config = server
            .request(Method::GET, &["config"], None)
            .await
            .expect("config");
        assert!(
            config
                .as_array()
                .expect("config entries")
                .iter()
                .any(|entry| {
                    entry["info"]["experimental"]["subagent_depth"] == 1
                        && entry["info"]["agents"]["general"]["permissions"][0]["effect"] == "deny"
                })
        );
        let mut effective = None;
        for _ in 0..100 {
            let agents = server
                .request(Method::GET, &["agent"], None)
                .await
                .expect("agents");
            effective = agents["data"].as_array().and_then(|agents| {
                agents
                    .iter()
                    .find(|agent| agent["id"] == "general")
                    .and_then(|agent| agent["permissions"].as_array())
                    .and_then(|rules| rules.iter().rev().find(|rule| rule["action"] == "subagent"))
                    .cloned()
            });
            if effective
                .as_ref()
                .is_some_and(|rule| rule["effect"] == "deny")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            effective.expect("general agent permission")["effect"],
            "deny"
        );
        server.shutdown().await.expect("stop restricted server");
        std::fs::remove_dir_all(home).expect("remove temporary home");
    }

    #[tokio::test]
    async fn external_connection_does_not_own_or_kill_server() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let responder = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 4096];
                let _ = stream.read(&mut buffer).await.unwrap();
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await
                    .unwrap();
            }
        });
        let server = OpenCodeServer::connect(&format!("http://{address}"), "", Path::new("/tmp"))
            .await
            .expect("connect to existing server");
        assert!(server.child.is_none());
        server
            .shutdown()
            .await
            .expect("external shutdown is a no-op");
        let response = reqwest::get(format!("http://{address}/api/info"))
            .await
            .expect("server remains available");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        responder.await.expect("responder completed");
    }
}
