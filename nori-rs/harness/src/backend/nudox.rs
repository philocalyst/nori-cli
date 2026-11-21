//! nudox, on by default: when the `nudox` command is installed, every session of every agent
//! gets its MCP server for the session's project ([docs/nudox.md]).
//!
//! The entry is whatever `nudox mcp-config --format json --project <cwd>` prints, run once per
//! project in the background and remembered for the life of the process. Nothing here waits on
//! it for long: resolution starts when the backend is prepared (long before a session exists),
//! and a session that is created while it is still running waits only for what is left of
//! [`GRACE`] counted from when it started, then goes on without nudox. The next session of that
//! project (a swap, a resume, a new conversation) finds the entry in the cache.
//!
//! Every session reaches the agent through `session_mcp_servers`, `spawn_and_relay` or a session
//! swap, and all three call [`add_to`], so there is one place that decides. A server the person
//! configured under the name `nudox` in `[mcp_servers]` (enabled or not) is theirs and is never
//! replaced, and `nudox = false` in `config.toml` turns the whole thing off.
//!
//! [docs/nudox.md]: ../../../docs/nudox.md

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

use nori_config::McpServerConfig;
use nori_protocol::acp::v1 as acp;
use tokio::sync::watch;
use tracing::debug;

/// The name of the MCP server, and of the command that prints its entry.
pub(super) const SERVER_NAME: &str = "nudox";

/// How long a session waits, counted from the start of resolution, for the entry.
const GRACE: Duration = Duration::from_millis(100);

/// How long `nudox mcp-config` may run.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);

/// When nudox was not found or failed, how long before the next session asks again.
const RETRY_AFTER: Duration = Duration::from_secs(60);

/// What resolution found: `None` while it runs, then the entry or `Some(None)` for "no nudox".
type Outcome = Option<Option<acp::McpServer>>;

struct Resolution {
    started: Instant,
    outcome: watch::Receiver<Outcome>,
}

static RESOLUTIONS: LazyLock<Mutex<HashMap<PathBuf, Resolution>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The resolution of `cwd`, started now when there is none (or when the last one found nothing a
/// while ago).
fn resolution(program: &OsStr, cwd: &Path) -> (Instant, watch::Receiver<Outcome>) {
    let mut resolutions = RESOLUTIONS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(known) = resolutions.get(cwd) {
        let failed = matches!(*known.outcome.borrow(), Some(None));
        if !failed || known.started.elapsed() < RETRY_AFTER {
            return (known.started, known.outcome.clone());
        }
    }
    let (sender, outcome) = watch::channel(None);
    let started = Instant::now();
    let project = cwd.to_path_buf();
    let program = program.to_owned();
    tokio::spawn(async move {
        let entry = resolve(&program, &project).await;
        sender.send_replace(Some(entry));
    });
    resolutions.insert(
        cwd.to_path_buf(),
        Resolution {
            started,
            outcome: outcome.clone(),
        },
    );
    (started, outcome)
}

/// Start finding the entry for `cwd` so it is ready by the time a session is created.
pub(super) fn warm(cwd: &Path, enabled: bool) {
    if enabled {
        drop(resolution(OsStr::new(SERVER_NAME), cwd));
    }
}

/// Add the nudox MCP server to `servers`, a session's list, unless it is switched off, the
/// person configured a server of that name, or the entry is not known yet (see the module docs).
pub(super) async fn add_to(
    servers: &mut Vec<acp::McpServer>,
    cwd: &Path,
    configured: &HashMap<String, McpServerConfig>,
    enabled: bool,
) {
    add_using(OsStr::new(SERVER_NAME), servers, cwd, configured, enabled).await;
}

/// [`add_to`] with the command that prints the entry named, so tests can supply their own.
async fn add_using(
    program: &OsStr,
    servers: &mut Vec<acp::McpServer>,
    cwd: &Path,
    configured: &HashMap<String, McpServerConfig>,
    enabled: bool,
) {
    if !enabled || configured.contains_key(SERVER_NAME) {
        return;
    }
    let (started, mut outcome) = resolution(program, cwd);
    let wait = GRACE.saturating_sub(started.elapsed());
    let found = match tokio::time::timeout(wait, outcome.wait_for(Option::is_some)).await {
        Ok(Ok(found)) => found.clone(),
        Ok(Err(_)) | Err(_) => None,
    };
    if let Some(Some(server)) = found {
        servers.push(server);
    }
}

/// Run `program mcp-config --format json --project <cwd>` and read its entry.
async fn resolve(program: &OsStr, cwd: &Path) -> Option<acp::McpServer> {
    let run = tokio::process::Command::new(program)
        .args(["mcp-config", "--format", "json", "--project"])
        .arg(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = match tokio::time::timeout(RESOLVE_TIMEOUT, run).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            debug!(%error, "nudox is not available");
            return None;
        }
        Err(_) => {
            debug!("nudox mcp-config timed out");
            return None;
        }
    };
    if !output.status.success() {
        debug!(status = ?output.status, "nudox mcp-config refused");
        return None;
    }
    let entry = parse_entry(&output.stdout);
    if entry.is_none() {
        debug!("nudox mcp-config printed no stdio server");
    }
    entry
}

/// The stdio server in `nudox mcp-config --format json`: `{"mcpServers": {"nudox": {"command",
/// "args", "env"}}}`.
fn parse_entry(json: &[u8]) -> Option<acp::McpServer> {
    let document: serde_json::Value = serde_json::from_slice(json).ok()?;
    let servers = document.get("mcpServers")?.as_object()?;
    let entry = servers
        .get(SERVER_NAME)
        .or_else(|| servers.values().next())?;
    if let Some(kind) = entry.get("type").and_then(serde_json::Value::as_str)
        && kind != "stdio"
    {
        return None;
    }
    let command = entry.get("command")?.as_str()?;
    let args = entry
        .get("args")
        .and_then(serde_json::Value::as_array)
        .map(|args| {
            args.iter()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let env = entry
        .get("env")
        .and_then(serde_json::Value::as_object)
        .map(|env| {
            env.iter()
                .filter_map(|(name, value)| {
                    Some(acp::EnvVariable::new(
                        name.clone(),
                        value.as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(acp::McpServer::Stdio(
        acp::McpServerStdio::new(SERVER_NAME, command)
            .args(args)
            .env(env),
    ))
}

/// What agents are told about nudox, with the tools they already have (appended to the guidance
/// of every session; it says what to do when the server is there).
pub(super) const INSTRUCTIONS: &str = "Code intelligence: when the `nudox` MCP server is available (its tools are `backend_search`, `backend_references`, `backend_graph`, `backend_outline`, ...), use it instead of grepping to answer \"where is this used\" and \"what calls this\": it returns exact source spans. Before you change a public declaration (an exported function, type, trait, command, config key or protocol message), ask for its impact: run the `impact` skill (`skill` with `name: \"impact\"`; no `arguments` means the working tree's changes, or name the declarations in `declarations`). It reports the declarations that changed, who uses them directly and transitively, the tests that cover them and the risks, each with nudox evidence, and it tells you when the project is not indexed. Asking for an impact is your consent to index this project, and only this project; never index another.";

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use pretty_assertions::assert_eq;

    use super::*;

    const PRINTED: &str = r#"{
  "mcpServers": {
    "nudox": {
      "type": "stdio",
      "command": "/bin/backend-mcp",
      "args": ["--project", "/work/app"]
    }
  }
}"#;

    fn stdio(command: &str, args: &[&str]) -> acp::McpServer {
        acp::McpServer::Stdio(
            acp::McpServerStdio::new(SERVER_NAME, command)
                .args(args.iter().map(ToString::to_string).collect::<Vec<_>>()),
        )
    }

    #[test]
    fn the_printed_entry_becomes_a_stdio_server() {
        assert_eq!(
            parse_entry(PRINTED.as_bytes()),
            Some(stdio("/bin/backend-mcp", &["--project", "/work/app"]))
        );
    }

    #[test]
    fn what_is_not_a_stdio_entry_is_ignored() {
        for printed in [
            "",
            "{}",
            "{\"slug\":\"usage\",\"answer\":\"fault\"}",
            "{\"mcpServers\":{\"nudox\":{\"type\":\"http\",\"url\":\"http://x\"}}}",
            "{\"mcpServers\":{\"nudox\":{\"type\":\"stdio\"}}}",
        ] {
            assert_eq!(parse_entry(printed.as_bytes()), None, "{printed:?}");
        }
    }

    fn fake_nudox(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("nudox");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make executable");
        path
    }

    #[tokio::test]
    async fn resolve_runs_mcp_config_for_the_project() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The script checks it is asked the right question and prints the project it was given.
        let program = fake_nudox(
            dir.path(),
            r#"[ "$1 $2 $3 $4" = "mcp-config --format json --project" ] || exit 64
printf '{"mcpServers":{"nudox":{"type":"stdio","command":"/bin/backend-mcp","args":["--project","%s"]}}}' "$5""#,
        );
        assert_eq!(
            resolve(program.as_os_str(), Path::new("/work/app")).await,
            Some(stdio("/bin/backend-mcp", &["--project", "/work/app"]))
        );
    }

    #[tokio::test]
    async fn a_missing_or_refusing_nudox_gives_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve(dir.path().join("absent").as_os_str(), Path::new("/work")).await,
            None
        );
        let refusing = fake_nudox(dir.path(), "exit 64");
        assert_eq!(
            resolve(refusing.as_os_str(), Path::new("/work")).await,
            None
        );
    }

    #[tokio::test]
    async fn a_session_gets_the_server_once_resolved_and_a_missing_nudox_adds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = fake_nudox(
            dir.path(),
            r#"printf '{"mcpServers":{"nudox":{"type":"stdio","command":"/bin/backend-mcp","args":["--project","%s"]}}}' "$5""#,
        );
        let none = HashMap::new();
        let mut servers = vec![stdio("other", &[])];
        // The entry is found in the background when the backend is prepared; by the time the
        // session is created it is there, so the session does not wait.
        let (_, mut resolved) = resolution(program.as_os_str(), Path::new("/work/one"));
        resolved.wait_for(Option::is_some).await.expect("resolved");
        add_using(
            program.as_os_str(),
            &mut servers,
            Path::new("/work/one"),
            &none,
            true,
        )
        .await;
        assert_eq!(
            servers,
            vec![
                stdio("other", &[]),
                stdio("/bin/backend-mcp", &["--project", "/work/one"])
            ]
        );

        let mut servers = Vec::new();
        let absent = dir.path().join("absent");
        add_using(
            absent.as_os_str(),
            &mut servers,
            Path::new("/work/two"),
            &none,
            true,
        )
        .await;
        assert_eq!(servers, Vec::new());
    }

    #[tokio::test]
    async fn opting_out_and_a_configured_server_leave_the_list_alone() {
        let mut servers = Vec::new();
        let none = HashMap::new();
        add_to(&mut servers, Path::new("/nowhere"), &none, false).await;
        assert_eq!(servers, Vec::new());
        let configured = HashMap::from([(
            SERVER_NAME.to_owned(),
            serde_json::from_str::<McpServerConfig>(r#"{"command":"mine","enabled":false}"#)
                .expect("a configured server"),
        )]);
        add_to(&mut servers, Path::new("/nowhere"), &configured, true).await;
        assert_eq!(servers, Vec::new());
    }
}
