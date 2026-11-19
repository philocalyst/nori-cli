//! Booting a world: the registry, the config, and the claim on the agent slugs.
//!
//! The agent registry is process-wide, so two worlds with an agent of the same slug cannot run at
//! once in one process; a world claims its slugs and waits for them. Worlds with different slugs
//! run side by side, and the registry lists the agents of every live world.

use std::collections::HashSet;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

use anyhow::Context;
use nori_config::AgentConfigToml;
use nori_fake_agent::AgentEntry;
use nori_fake_agent::ClockMode;
use tokio::sync::Notify;

use crate::approvals::Approvals;

/// A last say over the Nori configuration a session starts with.
pub type Configure = Box<dyn FnOnce(&mut nori_config::NoriConfig) + Send>;

/// How the agents of a world are run.
#[derive(Debug, Clone, Default)]
pub enum Transport {
    /// In this process, over an in-memory pipe (the default).
    #[default]
    InProcess,
    /// As separate processes speaking ACP over stdio: `<nori> agent serve fake ...`, registered
    /// as a local-distribution agent, so the harness spawns, pipes, reaps and kills them as it
    /// does a real agent. The wire tap's faults apply on the real pipe.
    Process {
        /// The `nori` binary to serve the fake agent with.
        nori: std::path::PathBuf,
    },
}

impl Transport {
    pub fn label(&self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::Process { .. } => "stdio process",
        }
    }
}

/// How to boot a world.
pub struct WorldOptions {
    /// How the agents run.
    pub transport: Transport,
    pub approvals: Approvals,
    /// The agent the session starts on; the world's default when absent.
    pub agent: Option<String>,
    /// How the fake agents' time passes. Virtual by default: deterministic and instant.
    pub clock: ClockMode,
    /// How long without any event counts as "nothing more is going to happen". It must outlast
    /// the harness's completion-wake batching (600 ms), or a settle can end between a child's
    /// result and the wake it causes.
    pub quiet: Duration,
    /// The longest any wait lasts before it is reported as stuck.
    pub timeout: Duration,
    /// Last say over the Nori configuration the session starts with.
    pub configure: Option<Configure>,
}

impl Default for WorldOptions {
    fn default() -> Self {
        Self {
            transport: Transport::InProcess,
            approvals: Approvals::Manual,
            agent: None,
            clock: ClockMode::Virtual,
            quiet: Duration::from_millis(1000),
            timeout: Duration::from_secs(60),
            configure: None,
        }
    }
}

struct Live {
    slugs: HashSet<String>,
    entries: Vec<(String, AgentConfigToml)>,
}

static LIVE: LazyLock<Mutex<Live>> = LazyLock::new(|| {
    Mutex::new(Live {
        slugs: HashSet::new(),
        entries: Vec::new(),
    })
});
static RELEASED: Notify = Notify::const_new();

/// A world's claim on its agent slugs and its entries in the registry. Released on drop.
pub(crate) struct Claim {
    slugs: Vec<String>,
}

fn config_entry(
    entry: &AgentEntry,
    process: Option<&ProcessAgent>,
) -> anyhow::Result<AgentConfigToml> {
    let text = match process {
        None => format!(
            "name = {:?}\nslug = {:?}\n[distribution.native]\nagent = {:?}\nconfig = {:?}\n",
            entry.name,
            entry.slug,
            nori_fake_agent::NAME,
            entry.config
        ),
        Some(process) => {
            let mut args = vec![
                "agent".to_owned(),
                "serve".to_owned(),
                nori_fake_agent::NAME.to_owned(),
            ];
            args.extend(entry.config.iter().cloned());
            format!(
                "name = {:?}\nslug = {:?}\n[distribution.local]\ncommand = {:?}\nargs = {:?}\n\
                 [distribution.local.env]\n{} = {:?}\n",
                entry.name,
                entry.slug,
                process.nori.display().to_string(),
                args,
                nori_fake_agent::RECORD_FILE_ENV,
                process.record_file.display().to_string(),
            )
        }
    };
    toml::from_str(&text).with_context(|| format!("agent entry for {}", entry.slug))
}

/// What running an agent as a process needs.
pub(crate) struct ProcessAgent {
    pub(crate) nori: std::path::PathBuf,
    /// The file the process appends its flight recorder to.
    pub(crate) record_file: std::path::PathBuf,
}

fn republish(live: &Live) {
    let agents = live
        .entries
        .iter()
        .map(|(_, entry)| entry.clone())
        .collect();
    if let Err(error) = nori_acp_host::registry::initialize_registry(agents) {
        tracing::error!("could not publish the agent registry: {error:#}");
    }
}

impl Claim {
    /// Wait until no live world has any of these agents, then take them.
    pub(crate) async fn take(
        entries: &[AgentEntry],
        process: Option<&ProcessAgent>,
        limit: Duration,
    ) -> anyhow::Result<Self> {
        let configs: Vec<(String, AgentConfigToml)> = entries
            .iter()
            .map(|entry| Ok((entry.slug.clone(), config_entry(entry, process)?)))
            .collect::<anyhow::Result<_>>()?;
        let wanted: Vec<String> = entries.iter().map(|entry| entry.slug.clone()).collect();
        let waiting = async {
            loop {
                let released = RELEASED.notified();
                tokio::pin!(released);
                released.as_mut().enable();
                {
                    let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
                    if wanted.iter().all(|slug| !live.slugs.contains(slug)) {
                        live.slugs.extend(wanted.iter().cloned());
                        live.entries.extend(configs.iter().cloned());
                        // Nothing real is offered, probed or warmed in a world.
                        nori_acp_host::registry::list_builtin_agents(false);
                        nori_acp_host::in_process::register(nori_fake_agent::registration());
                        republish(&live);
                        return;
                    }
                }
                released.await;
            }
        };
        tokio::time::timeout(limit, waiting).await.map_err(|_| {
            anyhow::anyhow!(
                "another world in this process holds the agents {}; worlds that share an agent \
                 slug run one at a time, and it did not finish within {limit:?}",
                wanted.join(", ")
            )
        })?;
        Ok(Self { slugs: wanted })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let mut live = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        for slug in &self.slugs {
            live.slugs.remove(slug);
        }
        live.entries.retain(|(slug, _)| !self.slugs.contains(slug));
        if live.slugs.is_empty() {
            // The last world is gone: real agents are listed again.
            nori_acp_host::registry::list_builtin_agents(true);
        }
        republish(&live);
        drop(live);
        RELEASED.notify_waiters();
    }
}
