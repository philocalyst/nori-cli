//! OpenCode in Nori's process: OpenCode's model gateway (OpenCode Zen) with an API key, run by
//! Nori's engine (`nori-engine`) against the gateway's APIs directly (`nori-llm`).
//!
//! Nothing of the OpenCode program is read or run: not its configuration, its database, its
//! sign-ins to other providers (a Claude subscription, Copilot, z.ai) or its plugins. The key is
//! the account's own, kept by Nori; the models are the gateway's, from models.dev. Other
//! providers are Nori's own agents, not OpenCode's. See `docs/native-agents.md`.

mod catalog;
mod env;
mod flavor;
mod key;
mod models;

use nori_acp_kit::Registration;
use nori_acp_kit::SpecOptions;
use nori_engine::EngineAgent;

/// The environment an OpenCode agent reads, from its spec, and where its key is kept: in the
/// account's home, else beside Nori's OpenCode sessions.
fn environment(spec: &nori_acp_kit::InProcessSpec) -> (env::Env, std::path::PathBuf) {
    let env = env::Env::inherited(spec.environment.clone());
    let key_file = match &spec.home {
        Some(home) => home.join(key::FILE),
        None => env::store_root(&env).join(key::FILE),
    };
    (env, key_file)
}

/// Offer OpenCode to the host as the native agent `opencode`. A prewarm reads the catalog a
/// session will need before it is opened.
pub fn registration() -> Registration {
    Registration::new("opencode", |spec| async move {
        SpecOptions::parse("OpenCode", &spec.config, &[])?;
        let (env, key_file) = environment(&spec);
        Ok(EngineAgent::new(flavor::OpenCodeFlavor::new(env, key_file)))
    })
    .with_auth_probe(|spec| async move {
        use nori_engine::Flavor;
        let (env, key_file) = environment(&spec);
        flavor::OpenCodeFlavor::new(env, key_file)
            .auth_status()
            .await
    })
    .with_prewarm(|spec, _cwd, _mcp_servers| {
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let (env, _) = environment(&spec);
            runtime.spawn(async move { catalog::Catalog::shared(&env).models().await });
        }
    })
}
