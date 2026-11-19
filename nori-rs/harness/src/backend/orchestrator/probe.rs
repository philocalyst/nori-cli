//! Keeping the catalog current: installed agents are started briefly, in the background, to read
//! the models and settings they offer.
//!
//! A probe is a bare session: no hooks, no MCP servers, no transcript. Each agent has at most
//! one probe at a time, and an agent whose entry is fresh enough is left alone, so asking often
//! costs nothing. Asking while a probe runs waits for that probe instead of starting another.
//! An agent whose probe failed is left alone too, for longer after each failure in a row, so an
//! agent that cannot start (OpenCode with no server running) costs one attempt per backoff period
//! however often the catalog is asked about.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use tokio::sync::watch;

use super::Seed;

/// How long an agent may take to start and open a session for its probe.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a user-visible re-check of one agent may take.
const REPROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Agents probed at once.
const PARALLEL_PROBES: usize = 3;

/// How long an agent is left alone after its first failed probe; each failure in a row doubles it.
const FIRST_BACKOFF: Duration = Duration::from_secs(30);
/// The longest an agent is left alone after failing.
const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Agents whose last probes failed: when to try again, and how many failed in a row. Cleared by
/// the first probe that succeeds.
static FAILED: LazyLock<Mutex<HashMap<ProbeKey, (Instant, i32)>>> = LazyLock::new(Default::default);

/// Agents with a probe running, each with what turns true when it ends.
type ProbeKey = (PathBuf, String);
static PROBING: LazyLock<Mutex<HashMap<ProbeKey, watch::Receiver<bool>>>> =
    LazyLock::new(Default::default);

/// Probe every installed agent whose catalog entry is older than `fresh_for`. Returns when they
/// have all answered or given up, including probes that were running already; the caller decides
/// how long to wait for that.
pub(crate) async fn refresh(
    config: Arc<crate::config::NoriConfig>,
    cli_version: String,
    fresh_for: Duration,
) {
    refresh_excluding(config, cli_version, fresh_for, None).await;
}

/// Refresh all stale installed agents except an agent whose primary session is being started.
/// Its advertised config is remembered by that session as soon as it starts.
pub(crate) async fn refresh_excluding(
    config: Arc<crate::config::NoriConfig>,
    cli_version: String,
    fresh_for: Duration,
    excluded_agent: Option<String>,
) {
    let now = super::catalog::now_seconds();
    let home = config
        .nori_home
        .canonicalize()
        .unwrap_or_else(|_| config.nori_home.clone());
    let permits = Arc::new(tokio::sync::Semaphore::new(PARALLEL_PROBES));
    let probes = crate::recovery::installed_agents(Some(&config.active_agent))
        .into_iter()
        .map(|agent| agent.agent_name)
        .filter(|agent| excluded_agent.as_deref() != Some(agent.as_str()))
        .filter(|agent| {
            let refreshed_at = super::catalog::remembered(&config.nori_home, agent).refreshed_at;
            now - refreshed_at >= fresh_for.as_secs() as i64
        })
        .filter(|agent| {
            let failed = FAILED.lock().unwrap_or_else(PoisonError::into_inner);
            failed
                .get(&(home.clone(), agent.clone()))
                .is_none_or(|(retry_at, _)| Instant::now() >= *retry_at)
        })
        .map(|agent| {
            let key = (home.clone(), agent.clone());
            let mut probing = PROBING.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(running) = probing.get(&key) {
                return running.clone();
            }
            let (ended_tx, ended) = watch::channel(false);
            probing.insert(key.clone(), ended.clone());
            drop(probing);
            let config = Arc::clone(&config);
            let cli_version = cli_version.clone();
            let permits = Arc::clone(&permits);
            tokio::spawn(async move {
                if let Ok(_permit) = permits.acquire().await {
                    let outcome =
                        tokio::time::timeout(PROBE_TIMEOUT, probe(&config, cli_version, &agent))
                            .await
                            .unwrap_or_else(|_| Err("timed out".to_owned()));
                    note_outcome(&key, &outcome);
                }
                PROBING
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&key);
                let _ = ended_tx.send(true);
            });
            ended
        })
        .map(|mut ended| async move {
            // A probe that was dropped without saying so has ended too.
            let _ = ended.wait_for(|ended| *ended).await;
        });
    futures::future::join_all(probes).await;
}

/// Ask `agent` again what it offers, now: the cheap check that a model which failed as
/// unavailable is still advertised. Waits for a probe already running instead of starting another.
pub(crate) async fn reprobe(
    config: Arc<crate::config::NoriConfig>,
    cli_version: String,
    agent: &str,
) -> Result<(), String> {
    let key = (
        config
            .nori_home
            .canonicalize()
            .unwrap_or_else(|_| config.nori_home.clone()),
        agent.to_owned(),
    );
    let outcome = tokio::time::timeout(REPROBE_TIMEOUT, probe(&config, cli_version, agent))
        .await
        .unwrap_or_else(|_| Err("timed out".to_owned()));
    note_outcome(&key, &outcome);
    outcome
}

/// Remember how a probe ended: a failure postpones the next background probe of that agent, a
/// success forgets earlier failures.
fn note_outcome(key: &ProbeKey, outcome: &Result<(), String>) {
    let mut failed = FAILED.lock().unwrap_or_else(PoisonError::into_inner);
    let Err(error) = outcome else {
        failed.remove(key);
        return;
    };
    let streak = failed.get(key).map_or(1, |(_, streak)| streak + 1);
    let backoff = FIRST_BACKOFF
        .saturating_mul(1 << (streak - 1).min(10))
        .min(MAX_BACKOFF);
    failed.insert(key.clone(), (Instant::now() + backoff, streak));
    tracing::debug!(
        agent = %key.1,
        %error,
        retry_in_secs = backoff.as_secs(),
        "could not read what the agent offers"
    );
}

/// Start `agent` in a bare session, remember what it advertises, and stop it.
async fn probe(
    config: &crate::config::NoriConfig,
    cli_version: String,
    agent: &str,
) -> Result<(), String> {
    let mut bare = config.clone();
    bare.active_agent = agent.to_owned();
    bare.mcp_servers.clear();
    bare.session_start_hooks.clear();
    bare.session_end_hooks.clear();
    bare.async_session_start_hooks.clear();
    bare.async_session_end_hooks.clear();
    let bare = Arc::new(bare);
    let spec = crate::runtime::AgentPrepareSpec {
        handoff: None,
        config: Arc::clone(&bare),
        cli_version: cli_version.clone(),
        session_context: None,
        initial_context: None,
    };
    // Catalog probes are not user conversations and do not need a journal.
    let seed = Seed::root(Arc::clone(&bare), cli_version).without_transcript();
    let prepared = crate::runtime::prepare_delegated_agent(spec, &seed)
        .await
        .map_err(|error| format!("{error:#}"))?;
    let mut session = crate::runtime::launch_session(crate::runtime::SessionLaunchSpec {
        agent: prepared,
        start: crate::runtime::SessionStart::New,
    });
    let result = loop {
        match session.events.recv().await {
            Some(SessionEvent::Nori(NoriEvent::SessionStarted(started))) => {
                super::catalog::remember(&config.nori_home, agent, &started.config_options);
                break Ok(());
            }
            Some(SessionEvent::Nori(NoriEvent::SessionEnded(ended))) => {
                break Err(ended
                    .message
                    .unwrap_or_else(|| "the session ended".to_owned()));
            }
            Some(_) => {}
            None => break Err("the agent exited".to_owned()),
        }
    };
    let _ = session.handle.close_session().await;
    let _ = session.handle.shutdown().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// A probe opens a bare session on the agent, reads what it advertises, and leaves no
    /// transcript behind.
    #[tokio::test]
    async fn a_probe_remembers_what_the_agent_offers() {
        let home = tempfile::tempdir().expect("home");
        let config = crate::config::NoriConfig {
            active_agent: "mock-model".to_owned(),
            cwd: home.path().to_path_buf(),
            nori_home: home.path().to_path_buf(),
            ..Default::default()
        };
        probe(&config, "probe-test".to_owned(), "mock-model-alt")
            .await
            .expect("probe the mock agent");
        let offer = super::super::catalog::remembered(home.path(), "mock-model-alt");
        let models: Vec<&str> = offer
            .models
            .iter()
            .map(|model| model.value.as_str())
            .collect();
        assert_eq!(models, ["mock-model-default", "mock-model-fast"]);
        assert!(offer.refreshed_at > 0);
        assert!(
            !home.path().join("transcripts").exists(),
            "a probe keeps no transcript"
        );
    }
}
