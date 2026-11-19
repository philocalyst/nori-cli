//! Accounts: a provider, a home (the directory its login lives in) and a name.
//!
//! ```toml
//! [[accounts]]
//! provider = "codex"       # claude | codex | gemini | opencode
//! name = "work"            # the agent is `codex@work`, shown as "Codex · work"
//! # home = "~/somewhere"   # default: <nori home>/accounts/codex/work
//! ```
//!
//! Each entry becomes one in-process agent whose home is set through the provider's own variable
//! ([`PROVIDERS`]), so any number of accounts of one provider share one binary and need no wrapper
//! script. They live beside `[[agents]]`; an `[[agents]]` wrapper is never rewritten, only read as
//! an account of its provider (see `nori_acp_host::accounts`).

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;

use crate::AgentConfigToml;
use crate::AgentDistributionToml;
use crate::NativeDistribution;

/// One `[[accounts]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AccountToml {
    pub provider: String,
    pub name: String,
    /// Where the account's login lives. Default: `<nori home>/accounts/<provider>/<name>`.
    #[serde(default)]
    pub home: Option<String>,
}

/// The environment variable a provider reads its home from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HomeVariable {
    pub name: &'static str,
    /// The directory inside an account's home the variable names (`""`: the home itself).
    pub subdir: &'static str,
}

/// What Nori knows about one provider's accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderAccounts {
    /// The id used in `[[accounts]]` and before the `@` of an agent slug.
    pub id: &'static str,
    pub title: &'static str,
    /// The built-in in-process agent that runs an account of this provider.
    pub native_agent: &'static str,
    /// How the provider is told which account to use. `None` is honest: the provider keeps one
    /// login in one place, so it has a single account.
    pub home: Option<HomeVariable>,
}

/// The one table of provider home variables.
pub const PROVIDERS: [ProviderAccounts; 5] = [
    ProviderAccounts {
        id: "claude",
        title: "Claude",
        native_agent: "claude",
        home: Some(HomeVariable {
            name: "CLAUDE_CONFIG_DIR",
            subdir: "",
        }),
    },
    ProviderAccounts {
        id: "codex",
        title: "Codex",
        native_agent: "codex",
        home: Some(HomeVariable {
            name: "CODEX_HOME",
            subdir: "",
        }),
    },
    ProviderAccounts {
        id: "gemini",
        title: "Gemini",
        native_agent: "gemini",
        home: Some(HomeVariable {
            name: "GEMINI_CLI_HOME",
            subdir: "",
        }),
    },
    ProviderAccounts {
        id: "opencode",
        title: "OpenCode",
        native_agent: "opencode",
        home: Some(HomeVariable {
            name: "XDG_DATA_HOME",
            subdir: "data",
        }),
    },
    ProviderAccounts {
        id: "antigravity",
        title: "Antigravity",
        native_agent: "antigravity-sdk",
        home: None,
    },
];

impl ProviderAccounts {
    pub fn find(id: &str) -> Option<&'static ProviderAccounts> {
        PROVIDERS.iter().find(|provider| provider.id == id)
    }

    /// The environment that points the provider at `home`; empty for a provider with one account.
    pub fn environment(&self, home: &Path) -> HashMap<String, String> {
        self.home
            .map(|variable| {
                let dir = if variable.subdir.is_empty() {
                    home.to_path_buf()
                } else {
                    home.join(variable.subdir)
                };
                (variable.name.to_owned(), dir.to_string_lossy().into_owned())
            })
            .into_iter()
            .collect()
    }
}

/// The agent slug of an account: `codex@work`.
pub fn account_slug(provider: &str, name: &str) -> String {
    format!("{provider}@{name}")
}

/// The provider and name of an account agent's slug; `None` for any other agent.
pub fn parse_account_slug(slug: &str) -> Option<(&'static ProviderAccounts, &str)> {
    let (provider, name) = slug.split_once('@')?;
    ProviderAccounts::find(provider).map(|provider| (provider, name))
}

/// Why `name` cannot name an account, if it cannot.
pub fn check_account_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid_start = chars.next().is_some_and(|c| c.is_ascii_alphanumeric());
    let valid_rest = chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !valid_start || !valid_rest || name.len() > 32 {
        return Err(format!(
            "{name:?} is not a usable account name: use up to 32 letters, digits, '-' or '_', starting with a letter or digit"
        ));
    }
    Ok(())
}

/// The default home of an account: `<nori home>/accounts/<provider>/<name>`.
pub fn default_account_home(nori_home: &Path, provider: &str, name: &str) -> PathBuf {
    nori_home.join("accounts").join(provider).join(name)
}

/// The agents the `[[accounts]]` of a config stand for, one in-process agent each.
pub fn expand_accounts(
    accounts: &[AccountToml],
    nori_home: &Path,
    agents: &[AgentConfigToml],
) -> Result<Vec<AgentConfigToml>> {
    let mut expanded: Vec<AgentConfigToml> = Vec::with_capacity(accounts.len());
    for account in accounts {
        let Some(provider) = ProviderAccounts::find(&account.provider) else {
            bail!(
                "[[accounts]] entry {:?}: unknown provider {:?} (use one of: {})",
                account.name,
                account.provider,
                PROVIDERS.map(|provider| provider.id).join(", ")
            );
        };
        if provider.home.is_none() {
            bail!(
                "[[accounts]] entry {:?}: {} keeps one login in one place, so it has a single account",
                account.name,
                provider.title
            );
        }
        check_account_name(&account.name).map_err(anyhow::Error::msg)?;
        let slug = account_slug(provider.id, &account.name);
        if expanded
            .iter()
            .chain(agents)
            .any(|agent| agent.slug == slug)
        {
            bail!(
                "[[accounts]] entry {slug:?} is listed twice, or an [[agents]] entry uses its slug"
            );
        }
        let home = match account.home.as_deref() {
            Some(home) => resolve_home(home, nori_home),
            None => default_account_home(nori_home, provider.id, &account.name),
        };
        expanded.push(AgentConfigToml {
            name: format!("{} · {}", provider.title, account.name),
            slug,
            distribution: AgentDistributionToml {
                native: Some(NativeDistribution {
                    agent: provider.native_agent.to_owned(),
                    home: Some(home.to_string_lossy().into_owned()),
                    config: Vec::new(),
                }),
                ..AgentDistributionToml::default()
            },
            context_window_size: None,
            auth_hint: Some(format!(
                "sign in to {} · {} from /providers",
                provider.title, account.name
            )),
            transcript_base_dir: None,
            model_override: None,
        });
    }
    Ok(expanded)
}

fn resolve_home(home: &str, nori_home: &Path) -> PathBuf {
    if let Some(rest) = home.strip_prefix("~/")
        && let Some(user_home) = dirs::home_dir()
    {
        return user_home.join(rest);
    }
    let path = PathBuf::from(home);
    if path.is_absolute() {
        path
    } else {
        nori_home.join(path)
    }
}
