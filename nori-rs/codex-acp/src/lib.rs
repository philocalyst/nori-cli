//! Codex as an in-process ACP agent for Nori, built on `nori-acp-kit`.
//!
//! Nori links Codex's core and runs it in its own process: its ACP client sits on the other end
//! of an in-memory pipe, so no process is started. The crate's one public entry point is
//! [`registration`]; the application collects it with the other agents' and hands it to the ACP
//! host.
//!
//! - `load`: the Codex configuration an in-process agent starts with.
//! - `env`: the environment a custom Codex adapter would have read, folded into overrides.
//! - `codex_agent`: the [`nori_acp_kit::NativeAgent`] over the Codex thread manager.
//! - `thread`: one session, and the translation of Codex's events into ACP updates.
//! - `host_tools`: Nori's host tools as Codex dynamic tools.
#![deny(clippy::print_stdout, clippy::print_stderr)]

mod codex_agent;
mod env;
mod host_tools;
mod load;
mod thread;

use nori_acp_kit::Registration;

/// Codex, as a `native = { agent = "codex" }` distribution.
///
/// `home` is the account's `CODEX_HOME` (a process-wide variable cannot tell two accounts
/// apart); `config` holds `key=value` overrides as `codex -c` takes them.
pub fn registration() -> Registration {
    Registration::new("codex", |spec| async move {
        load::load_agent(spec.home, spec.config).await
    })
    .with_resolver(env::fold_environment)
}
