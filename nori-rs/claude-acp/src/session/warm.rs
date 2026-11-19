//! Claude Code started ahead of time.
//!
//! Starting the `claude` CLI is most of what a new session costs. A warm session is a whole
//! session whose CLI is already running and connected, waiting for a session request it fits:
//! the same working directory, driver configuration and client MCP servers, and host tools or
//! not. A new session that fits takes it, and the driver starts the next one in the background,
//! so switching to Claude, starting over, or delegating a task to Claude finds its CLI already up.
//! Loads, resumes and forks start the CLI with arguments only they know, so they never take one.
//!
//! The pool is process-wide: an agent switch opens a new ACP connection, and the warm session
//! must outlive the connection that started it.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use nori_acp_kit::schema::McpServer;

use super::Session;
use crate::mcp::HostToolsBinding;
use crate::mcp::SessionTools;
use crate::types::AgentConfig;

/// A warm session older than this is not used: the user's settings may have changed under it.
const MAX_AGE: Duration = Duration::from_secs(30 * 60);
/// Warm sessions kept at once, across every key. Each is an idle `claude` process.
const MAX_WARM: usize = 2;

/// What a session must share with a warm one to take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WarmKey {
    cwd: PathBuf,
    config: String,
    mcp_servers: String,
    host_tools: bool,
}

impl WarmKey {
    pub fn new(cwd: &Path, config: &AgentConfig, tools: &SessionTools) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
            // Every field shapes the CLI's arguments or environment.
            config: format!("{config:?}"),
            mcp_servers: serde_json::to_string(&tools.mcp_servers).unwrap_or_default(),
            host_tools: tools.host_tools.is_some(),
        }
    }
}

struct Warm {
    key: WarmKey,
    session: Arc<Session>,
    started: Instant,
}

#[derive(Default)]
struct Pool {
    ready: Vec<Warm>,
    starting: Vec<WarmKey>,
}

fn pool() -> &'static Mutex<Pool> {
    static POOL: OnceLock<Mutex<Pool>> = OnceLock::new();
    POOL.get_or_init(Default::default)
}

/// The warm session that fits `key`, if one is ready. Expired ones are stopped on the way.
pub fn claim(key: &WarmKey) -> Option<Arc<Session>> {
    let (claimed, expired) = {
        let mut pool = pool().lock().unwrap_or_else(PoisonError::into_inner);
        let (fresh, expired): (Vec<Warm>, Vec<Warm>) = std::mem::take(&mut pool.ready)
            .into_iter()
            .partition(|warm| warm.started.elapsed() < MAX_AGE && warm.session.is_connected());
        pool.ready = fresh;
        let claimed = pool
            .ready
            .iter()
            .position(|warm| &warm.key == key)
            .map(|index| pool.ready.swap_remove(index).session);
        (claimed, expired)
    };
    stop(expired);
    claimed
}

/// Start a session that fits `key` in the background, unless one is warm or starting already.
/// `CLAUDE_ACP_NO_WARM=1` turns warm sessions off, to compare.
pub fn refill(key: WarmKey, cwd: PathBuf, config: AgentConfig, mcp_servers: Vec<McpServer>) {
    if super::session::env_flag("CLAUDE_ACP_NO_WARM") {
        return;
    }
    {
        let mut pool = pool().lock().unwrap_or_else(PoisonError::into_inner);
        if pool.starting.contains(&key) || pool.ready.iter().any(|warm| warm.key == key) {
            return;
        }
        pool.starting.push(key.clone());
    }
    tokio::spawn(async move {
        let tools = SessionTools {
            mcp_servers,
            host_tools: key
                .host_tools
                .then(|| Arc::new(HostToolsBinding::default())),
        };
        let session_id = uuid::Uuid::new_v4().to_string();
        let started = match Session::new(session_id, cwd, &config, None, &tools) {
            Ok(session) => match session.connect().await {
                Ok(()) => Some(session),
                Err(error) => {
                    tracing::debug!(%error, "A warm Claude session did not start");
                    None
                }
            },
            Err(error) => {
                tracing::debug!(%error, "A warm Claude session could not be made");
                None
            }
        };
        let evicted = {
            let mut pool = pool().lock().unwrap_or_else(PoisonError::into_inner);
            pool.starting.retain(|starting| starting != &key);
            let Some(session) = started else {
                return;
            };
            pool.ready.push(Warm {
                key,
                session,
                started: Instant::now(),
            });
            // The oldest go first.
            let excess = pool.ready.len().saturating_sub(MAX_WARM);
            pool.ready.drain(..excess).collect::<Vec<_>>()
        };
        stop(evicted);
    });
}

/// Stop the CLIs of sessions nobody will use.
fn stop(sessions: Vec<Warm>) {
    for warm in sessions {
        tokio::spawn(async move {
            if let Err(error) = warm.session.cleanup().await {
                tracing::debug!(%error, "Could not stop a warm Claude session");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn keys_differ_by_everything_that_shapes_the_cli() {
        let config = AgentConfig::default();
        let tools = SessionTools::default();
        let key = WarmKey::new(Path::new("/repo"), &config, &tools);
        assert_eq!(key, WarmKey::new(Path::new("/repo"), &config, &tools));
        assert_ne!(key, WarmKey::new(Path::new("/other"), &config, &tools));

        let with_model = AgentConfig {
            model: Some("opus".to_owned()),
            ..AgentConfig::default()
        };
        assert_ne!(key, WarmKey::new(Path::new("/repo"), &with_model, &tools));

        let with_host_tools = SessionTools {
            host_tools: Some(Arc::new(HostToolsBinding::default())),
            ..SessionTools::default()
        };
        assert_ne!(
            key,
            WarmKey::new(Path::new("/repo"), &config, &with_host_tools)
        );
    }

    #[test]
    fn nothing_is_claimed_from_an_empty_pool() {
        let key = WarmKey::new(
            Path::new("/nowhere-warm"),
            &AgentConfig::default(),
            &SessionTools::default(),
        );
        assert!(claim(&key).is_none());
    }
}
