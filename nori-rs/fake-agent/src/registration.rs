//! Offering the fake agents to Nori: the `fake` registration, the options it takes, and the
//! config entries that make a world's agents appear in a registry.

use std::io::Error;
use std::io::ErrorKind;
use std::sync::Arc;

use nori_acp_kit::Host;
use nori_acp_kit::InProcessSpec;
use nori_acp_kit::Registration;
use nori_acp_kit::SpecOptions;
use nori_acp_kit::serve_over_pipe_as;

use crate::agent::FakeAgent;
use crate::clock::ClockMode;
use crate::runtime::WorldRuntime;
use crate::wire::TapContext;
use crate::wire::TapEnd;
use crate::wire::interpose;
use crate::world::World;

/// What a `native = { agent = "fake" }` distribution calls the fake agents.
pub const NAME: &str = "fake";

/// The `key=value` options the fake agent takes.
pub const OPTIONS: [&str; 3] = ["world", "agent", "clock"];

/// The `fake` registration. Options: `world=<file or mem:N>`, `agent=<slug>` (the world's
/// default when absent), `clock=virtual|real|x10` (virtual when Nori runs the agent in its own
/// process, real when served to an external client).
pub fn registration() -> Registration {
    Registration {
        name: NAME,
        launch: Arc::new(|spec, pipe, host| Box::pin(serve_spec(spec, pipe, host))),
        prewarm: None,
        resolve: None,
        // A fake agent is signed in by its world (`startup.requires_auth`), not probed.
        auth_probe: None,
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorKind::InvalidInput, message.into())
}

async fn serve_spec(
    spec: InProcessSpec,
    pipe: tokio::io::DuplexStream,
    host: Host,
) -> std::io::Result<()> {
    let options = SpecOptions::parse(NAME, &spec.config, &OPTIONS)?;
    let reference = options.get("world").ok_or_else(|| {
        invalid("the fake agent needs `world=<file>`: a Pkl or JSON world script")
    })?;
    let mode = match options.get("clock") {
        Some(clock) => ClockMode::parse(clock).map_err(invalid)?,
        None => match host {
            Host::Nori => ClockMode::Virtual,
            Host::External => ClockMode::Real(1.0),
        },
    };
    let runtime = WorldRuntime::resolve(reference, mode).map_err(Error::other)?;
    let slug = match options.get("agent") {
        Some(slug) => slug.to_owned(),
        None => runtime
            .world
            .default_agent()
            .map(|agent| agent.slug.clone())
            .ok_or_else(|| invalid("the world defines no agents"))?,
    };
    let agent_def = runtime.world.agent(&slug).ok_or_else(|| {
        invalid(format!(
            "the world has no agent {slug:?}; it has: {}",
            runtime.world.slugs().join(", ")
        ))
    })?;
    let (wire, kit_end, tap) = interpose(
        pipe,
        TapContext {
            agent: slug.clone(),
            recorder: runtime.recorder.clone(),
            clock: runtime.clock.clone(),
            initialize_delay: agent_def.startup.initialize_delay.0,
            initialize_error: agent_def.startup.initialize_error.clone(),
            initialize_never: agent_def.startup.hang_initialize,
            external: host == Host::External,
        },
    );
    let agent = FakeAgent::new(runtime, &slug, wire).map_err(Error::other)?;
    let (served, ended) = tokio::join!(serve_over_pipe_as(kit_end, agent, host), tap.run());
    if let TapEnd::Crashed {
        exit_code,
        message,
        sigkill,
    } = ended
        && host == Host::External
    {
        // A served process dies like the real thing: with its code, after saying why, or
        // without a word when it is killed.
        if let Some(message) = message {
            eprintln!("{message}");
        }
        #[cfg(unix)]
        if sigkill {
            // SAFETY: raising a signal at oneself has no memory-safety preconditions.
            unsafe {
                libc::raise(libc::SIGKILL);
            }
        }
        let _ = sigkill;
        std::process::exit(i32::try_from(exit_code).unwrap_or(1));
    }
    served
}

/// One agent of a world as the registry and config know it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEntry {
    pub slug: String,
    /// What the picker shows.
    pub name: String,
    /// The `key=value` options of its `native` distribution.
    pub config: Vec<String>,
}

impl World {
    /// The registry entries for this world's agents, given the `world=` reference they share (a
    /// file path, or what [`InstalledWorld::reference`] returns). Every entry is a `native`
    /// distribution of [`NAME`].
    pub fn agent_entries(&self, reference: &str, clock: Option<&str>) -> Vec<AgentEntry> {
        self.agents
            .iter()
            .map(|agent| {
                let mut config = vec![
                    format!("world={reference}"),
                    format!("agent={}", agent.slug),
                ];
                if let Some(clock) = clock {
                    config.push(format!("clock={clock}"));
                }
                AgentEntry {
                    slug: agent.slug.clone(),
                    name: agent.title().to_owned(),
                    config,
                }
            })
            .collect()
    }
}

/// A world installed in this process for the `world=` option to find, uninstalled on drop.
pub struct InstalledWorld {
    runtime: Arc<WorldRuntime>,
    reference: String,
}

impl InstalledWorld {
    /// Install `world` with a fresh runtime (clock, recorder, sessions).
    pub fn new(world: World, mode: ClockMode) -> Self {
        let runtime = WorldRuntime::new(world, mode);
        let reference = runtime.install();
        Self { runtime, reference }
    }

    /// The `world=` value of the agents' options.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn runtime(&self) -> &Arc<WorldRuntime> {
        &self.runtime
    }

    pub fn world(&self) -> &World {
        &self.runtime.world
    }

    pub fn entries(&self) -> Vec<AgentEntry> {
        self.runtime.world.agent_entries(&self.reference, None)
    }
}

impl Drop for InstalledWorld {
    fn drop(&mut self) {
        WorldRuntime::uninstall(&self.reference);
    }
}
