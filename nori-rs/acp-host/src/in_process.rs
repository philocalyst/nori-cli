//! Agents that run inside Nori's own process.
//!
//! An ACP agent normally is a program Nori starts and talks to over its stdin and stdout. A
//! built-in one is a library instead: Nori gives it one end of an in-memory pipe, keeps the other
//! end for its own ACP client, and no process is started. The agents live in crates `acp-host`
//! does not depend on, each built on `nori-acp-kit`; the application hands the host each crate's
//! [`Registration`] at startup, and a distribution of `native = { agent = "..." }` selects one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::RwLock;

pub use nori_acp_kit::Host;
pub use nori_acp_kit::InProcessSpec;
pub use nori_acp_kit::Launcher;
pub use nori_acp_kit::Prewarmer;
pub use nori_acp_kit::Registration;

fn registrations() -> &'static RwLock<HashMap<&'static str, Registration>> {
    static REGISTRATIONS: OnceLock<RwLock<HashMap<&'static str, Registration>>> = OnceLock::new();
    REGISTRATIONS.get_or_init(Default::default)
}

/// Make a built-in agent available to `native` distributions, replacing an earlier registration
/// of the same name.
pub fn register(registration: Registration) {
    registrations()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(registration.name, registration);
}

fn registration(name: &str) -> Option<Registration> {
    registrations()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(name)
        .cloned()
}

/// The launcher registered for `name`, if this build has one.
pub fn in_process_launcher(name: &str) -> Option<Launcher> {
    registration(name).map(|registration| registration.launch)
}

/// Whether this build can run the built-in agent `name`.
pub fn in_process_available(name: &str) -> bool {
    registration(name).is_some()
}

/// Have the built-in agent `config` runs start a session for `cwd` ahead of time, so switching to
/// it later is instant. Does nothing for an agent that is not built in or cannot prewarm.
pub fn prewarm(
    config: &crate::registry::AcpAgentConfig,
    cwd: PathBuf,
    mcp_servers: serde_json::Value,
) {
    let Some(spec) = &config.in_process else {
        return;
    };
    let Some(registration) = registration(&spec.agent) else {
        return;
    };
    let spec = with_environment(spec, &config.env);
    // An agent that can say who it is signed in as without starting does so now, so the
    // providers panel shows its login before any session of it runs.
    if let (Some(probe), Ok(runtime)) = (
        registration.auth_probe.clone(),
        tokio::runtime::Handle::try_current(),
    ) {
        let slug = config.provider_slug.clone();
        let probed = spec.clone();
        runtime.spawn(async move {
            let status = probe(probed).await;
            crate::accounts::record_identity(&slug, &status);
        });
    }
    if let Some(prewarm) = registration.prewarm.clone() {
        prewarm(spec, cwd, mcp_servers);
    }
}

/// Who the built-in agent `config` runs is signed in as, from its stored sign-ins and without
/// starting it. `None` when the agent is not built in or cannot say.
pub async fn probe_auth(
    config: &crate::registry::AcpAgentConfig,
) -> Option<nori_acp_kit::AuthStatus> {
    let spec = config.in_process.as_ref()?;
    let probe = registration(&spec.agent)?.auth_probe?;
    Some(probe(with_environment(spec, &config.env)).await)
}

/// `spec` with the environment an adapter would have read turned into explicit options, since a
/// process shared with other agents has no per-agent environment. How is the agent's own
/// business: it registered the resolver that does it.
pub(crate) fn with_environment(
    spec: &InProcessSpec,
    env: &HashMap<String, String>,
) -> InProcessSpec {
    let mut spec = spec.clone();
    spec.environment.extend(env.clone());
    // An account's home reaches the agent through the provider's own variable.
    if let Some(home) = &spec.home
        && let Some(provider) = nori_config::PROVIDERS
            .iter()
            .find(|provider| provider.native_agent == spec.agent)
    {
        for (name, value) in provider.environment(home) {
            spec.environment.entry(name).or_insert(value);
        }
    }
    match registration(&spec.agent).and_then(|registration| registration.resolve) {
        Some(resolve) => resolve(spec, env),
        None => spec,
    }
}

/// The built-in agents this build has, by name.
pub fn available_agents() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = registrations()
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .keys()
        .copied()
        .collect();
    names.sort_unstable();
    names
}

/// Serve the built-in agent `spec.agent` over this process's stdin and stdout, so any ACP client
/// can run it. `environment` is what a custom adapter would have read (the process environment,
/// plus overrides); the agent's resolver folds it into options. Returns when the client closes
/// stdin and the agent has shut down.
pub async fn serve_stdio(
    spec: InProcessSpec,
    environment: &HashMap<String, String>,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let Some(registration) = registration(&spec.agent) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "this build of Nori has no built-in agent {:?}; it has: {}",
                spec.agent,
                available_agents().join(", ")
            ),
        ));
    };
    let spec = with_environment(&spec, environment);
    let (host_end, agent_end) = tokio::io::duplex(1 << 16);
    let (mut from_agent, mut to_agent) = tokio::io::split(host_end);
    let inbound = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let copied = tokio::io::copy(&mut stdin, &mut to_agent).await;
        // End of stdin is the client leaving: let the agent see the pipe close.
        let _ = to_agent.shutdown().await;
        copied
    });
    let outbound = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        tokio::io::copy(&mut from_agent, &mut stdout).await?;
        stdout.flush().await
    });
    let served = (registration.launch)(spec, agent_end, Host::External).await;
    inbound.abort();
    // The agent's end of the pipe is closed, so the output drains and ends.
    let drained = outbound.await.map_err(std::io::Error::other)?;
    served.and(drained)
}
