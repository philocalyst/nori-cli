//! Codex as an in-process ACP agent for Nori, built on `nori-acp-kit`.
//!
//! Codex runs on Nori's own engine (`nori-engine`) in Nori's process: its ACP client sits on the
//! other end of an in-memory pipe, so no process is started and no Codex core is linked. What is
//! Codex's is its sign-in, configuration, model catalog, prompts and tool shapes. The crate's one
//! public entry point is [`registration`]; the application collects it with the other agents' and
//! hands it to the ACP host.
//!
//! - `env`: the environment a custom Codex adapter would have read, folded into overrides.
//! - `flavor`: the engine flavor (see its module docs).
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod env;
mod flavor;

use nori_acp_kit::Registration;

/// Codex, as a `native = { agent = "codex" }` distribution.
///
/// `home` is the account's `CODEX_HOME` (a process-wide variable cannot tell two accounts
/// apart); `config` holds `key=value` overrides as `codex -c` takes them.
pub fn registration() -> Registration {
    Registration::new("codex", |spec| async move {
        Ok(nori_engine::EngineAgent::new(
            flavor::CodexFlavor::launch(spec).await?,
        ))
    })
    .with_resolver(env::fold_environment)
    // Who the account is signed in as, read from its stored login without starting the agent.
    .with_auth_probe(|spec| async move {
        use nori_engine::Flavor;
        match flavor::CodexFlavor::launch(spec).await {
            Ok(codex) => codex.auth_status().await,
            Err(error) => nori_acp_kit::AuthStatus::unknown(format!(
                "Codex's stored sign-in could not be read: {error}"
            )),
        }
    })
}
