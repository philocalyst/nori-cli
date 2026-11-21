//! Accounts as a concept: a provider, a home and a name.
//!
//! An account is one login of one provider. Nori meets accounts three ways, and reads them all
//! into one [`Account`]:
//!
//! - **default**: the built-in agent, signed in wherever the provider keeps its own login;
//! - **native**: an `[[accounts]]` entry, an in-process agent whose home Nori sets through the
//!   provider's own variable (`nori_config::PROVIDERS`), slug `codex@work`;
//! - **wrapper**: an `[[agents]]` entry that launches the provider through a script that points it
//!   at its home (the nix-generated `nori-codex2-acp`). It is read, never rewritten.
//!
//! The agent's slug is the account's identity everywhere it is counted (usage, headroom,
//! routing): `codex@work` for a native account, the entry's own slug for the others.

mod create;
pub mod gemini_cli;
mod identity;
mod launch;
mod login;
pub mod status;
mod unwrap;
mod wrapper;

use std::path::PathBuf;

use nori_config::PROVIDERS;
use nori_config::ProviderAccounts;
use nori_config::ResolvedDistribution;

pub use create::AddPlan;
pub use create::add_native_account;
pub use create::plan_add;
pub use identity::AccountIdentity;
pub use identity::identity_of;
pub use identity::record_identity;
pub use launch::LoginProgram;
pub use launch::default_environment;
pub use launch::login_program;
pub use launch::native_environment;
pub use login::Answer;
pub use login::Capture;
pub use login::Secret;
pub use login::SignInAsk;
pub use login::SignInHow;
pub use login::SignInMethod;
pub use login::methods_for;
pub use login::store_claude_setting;
pub use status::AccountStatus;
pub use unwrap::Unwrapped;
pub use unwrap::unwrap_executable;

use crate::registry::AgentKind;
use crate::registry::RegisteredAgent;

/// Where an account's login lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Home {
    /// The provider's own default location.
    Default,
    /// A directory the provider is pointed at.
    At(PathBuf),
}

/// How Nori starts an account's agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The built-in agent as it ships.
    Default,
    /// An `[[accounts]]` entry, run inside Nori.
    Native,
    /// An `[[agents]]` entry that launches through `launcher`. When the launcher sets the
    /// provider's home itself, Nori cannot give the agent another one.
    Wrapper { launcher: String, sets_home: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The agent's slug: `codex@work`, `claude2`, `claude-code`.
    pub slug: String,
    pub provider: &'static ProviderAccounts,
    /// The account's own name: `work`, or `default` for the built-in agent.
    pub name: String,
    /// What a person sees: `Codex · work`.
    pub display_name: String,
    pub origin: Origin,
    pub home: Home,
}

impl Account {
    /// Whether this is the provider's own, unconfigured login.
    pub fn is_default(&self) -> bool {
        self.origin == Origin::Default
    }
}

/// The provider an agent kind belongs to.
pub fn provider_of(kind: AgentKind) -> &'static ProviderAccounts {
    let id = match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
        AgentKind::OpenCode => "opencode",
        AgentKind::Antigravity => "antigravity",
    };
    ProviderAccounts::find(id).unwrap_or(&PROVIDERS[0])
}

/// The command that installs an agent's CLI, when Nori knows a better one than
/// `npm install -g <package>`: the one that suits this machine.
pub fn install_command(kind: AgentKind) -> Option<String> {
    match kind {
        AgentKind::Gemini => Some(gemini_cli::install_command()),
        AgentKind::ClaudeCode | AgentKind::Codex | AgentKind::OpenCode | AgentKind::Antigravity => {
            None
        }
    }
}

/// The kind of agent a provider's accounts run.
pub fn kind_of(provider: &ProviderAccounts) -> AgentKind {
    match provider.id {
        "claude" => AgentKind::ClaudeCode,
        "codex" => AgentKind::Codex,
        "gemini" => AgentKind::Gemini,
        "opencode" => AgentKind::OpenCode,
        _ => AgentKind::Antigravity,
    }
}

/// The kind of agent a native account's slug names, when it is one.
pub fn kind_of_slug(slug: &str) -> Option<AgentKind> {
    nori_config::parse_account_slug(slug).map(|(provider, _)| kind_of(provider))
}

/// Every account the registry knows, in registry order.
pub fn list() -> Vec<Account> {
    crate::registry::registered_agents()
        .iter()
        .filter_map(read_account)
        .collect()
}

/// The account behind an agent slug.
pub fn find(slug: &str) -> Option<Account> {
    let slug = slug.to_lowercase();
    crate::registry::registered_agents()
        .iter()
        .find(|agent| agent.slug == slug)
        .and_then(read_account)
}

fn read_account(agent: &RegisteredAgent) -> Option<Account> {
    let Some(distribution) = &agent.distribution else {
        let kind = agent.kind?;
        return Some(Account {
            slug: agent.slug.clone(),
            provider: provider_of(kind),
            name: "default".to_owned(),
            display_name: agent.name.clone(),
            origin: Origin::Default,
            home: Home::Default,
        });
    };
    if let Some((provider, name)) = nori_config::parse_account_slug(&agent.slug)
        && let ResolvedDistribution::Native { home, .. } = distribution
    {
        return Some(Account {
            slug: agent.slug.clone(),
            provider,
            name: name.to_owned(),
            display_name: agent.name.clone(),
            origin: Origin::Native,
            home: home.clone().map_or(Home::Default, Home::At),
        });
    }
    let ResolvedDistribution::Local { command, env, .. } = distribution else {
        return None;
    };
    let scan = wrapper::scan(&agent.slug, command, env)?;
    Some(Account {
        slug: agent.slug.clone(),
        provider: scan.provider,
        name: agent.slug.clone(),
        display_name: agent.name.clone(),
        origin: Origin::Wrapper {
            launcher: scan.launcher,
            sets_home: scan.home.is_some(),
        },
        home: scan.home.map_or(Home::Default, Home::At),
    })
}
