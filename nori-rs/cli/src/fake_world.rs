//! `nori --world <file>`: the TUI against a world of fake agents (`nori-fake-agent`), with no
//! network and nothing real behind it.
//!
//! The world's agents are registered as in-process agents and become the whole agent registry;
//! the session starts on the world's default agent. The fake agents are also served by
//! `nori agent serve fake world=<file> [agent=<slug>] [clock=real]` for any ACP client, and by
//! `scripts/rex-scenario` for captures.

use std::path::Path;

use anyhow::Context;
use nori_acp_host::in_process;
use nori_config::AgentConfigToml;
use nori_fake_agent::World;

/// Make the fake agents available to `native = { agent = "fake" }` distributions.
pub fn register() {
    in_process::register(nori_fake_agent::registration());
}

/// Point the session `interactive` describes at the world in `path`.
pub fn apply(path: &Path, interactive: &mut nori_tui::Cli) -> anyhow::Result<()> {
    let world = World::from_path(path)?;
    let reference = path
        .canonicalize()
        .with_context(|| format!("world {}", path.display()))?
        .display()
        .to_string();
    // Nothing real is offered, probed or warmed in a world.
    nori_acp_host::registry::list_builtin_agents(false);
    // A person is watching, so time passes as the world says (real by default); `virtual` plays
    // a script in a flash. `NORI_WORLD_CLOCK=real|x30|virtual` overrides the world.
    let clock = std::env::var("NORI_WORLD_CLOCK")
        .ok()
        .or_else(|| world.clock.clone())
        .unwrap_or_else(|| "real".to_owned());
    interactive.extra_agents = world
        .agent_entries(&reference, Some(&clock))
        .iter()
        .map(|entry| {
            let text = format!(
                "name = {:?}\nslug = {:?}\n[distribution.native]\nagent = {:?}\nconfig = {:?}\n",
                entry.name,
                entry.slug,
                nori_fake_agent::NAME,
                entry.config
            );
            toml::from_str::<AgentConfigToml>(&text)
                .with_context(|| format!("agent entry for {}", entry.slug))
        })
        .collect::<anyhow::Result<_>>()?;
    if interactive.agent.is_none() {
        interactive.agent = world.default_agent().map(|agent| agent.slug.clone());
    }
    if let Some(policy) = &world.policy {
        install_policy(&world.name, policy)?;
    }
    Ok(())
}

/// The world's recovery policy goes in `$NORI_HOME/init.pkl`, which a person's own file is never
/// replaced by: with `NORI_HOME` unset (the person's real home) or an `init.pkl` already there,
/// it is not applied, and the person is told.
fn install_policy(world: &str, policy: &str) -> anyhow::Result<()> {
    let Some(home) = std::env::var_os("NORI_HOME").map(std::path::PathBuf::from) else {
        eprintln!(
            "world {world:?} ships a recovery policy; run with NORI_HOME set to a scratch \
             directory to apply it (your own home is never written)"
        );
        return Ok(());
    };
    let init = home.join("init.pkl");
    if init.exists() {
        eprintln!(
            "world {world:?} ships a recovery policy, but {} exists and is kept",
            init.display()
        );
        return Ok(());
    }
    std::fs::create_dir_all(&home).with_context(|| format!("create {}", home.display()))?;
    std::fs::write(&init, policy).with_context(|| format!("write {}", init.display()))
}
