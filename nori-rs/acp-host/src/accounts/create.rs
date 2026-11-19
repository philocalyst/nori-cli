//! Adding an account: the plan for a provider, and creating a native one.

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use nori_config::NoriConfigEdits;
use nori_config::ProviderAccounts;

use super::Account;
use super::Origin;

/// What "Add another account" does for a provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddPlan {
    /// Nori runs the new account itself, inside its own process.
    Native,
    /// It cannot, and why, in words that say what to do instead.
    Unavailable(String),
}

/// How an account of `provider` can be added in this build.
pub fn plan_add(provider: &ProviderAccounts) -> AddPlan {
    let Some(variable) = provider.home else {
        return AddPlan::Unavailable(format!(
            "{} keeps its login in one place, so it has a single account.",
            provider.title
        ));
    };
    if crate::in_process::in_process_available(provider.native_agent) {
        return match super::launch::executable_problem(provider) {
            Some(problem) => AddPlan::Unavailable(format!("{problem}.")),
            None => AddPlan::Native,
        };
    }
    let wrappers: Vec<Account> = super::list()
        .into_iter()
        .filter(|account| account.provider.id == provider.id)
        .filter(|account| matches!(account.origin, Origin::Wrapper { .. }))
        .collect();
    let how = format!(
        "add another launcher in your nix config that sets {} to the new account's directory, \
         and list it under [[agents]]",
        variable.name
    );
    AddPlan::Unavailable(match wrappers.first().map(|account| &account.origin) {
        Some(Origin::Wrapper {
            launcher,
            sets_home: true,
        }) => format!(
            "{} runs through `{launcher}`, which sets {} itself, so Nori cannot give it another \
             account. This build has no built-in {} agent: {how}.",
            provider.title, variable.name, provider.title
        ),
        Some(Origin::Wrapper { .. } | Origin::Default | Origin::Native) | None => format!(
            "This build has no built-in {} agent, so it cannot run another account itself: {how}.",
            provider.title
        ),
    })
}

/// Create the account `name` of `provider`: its home under `<nori home>/accounts/`, its
/// `[[accounts]]` entry in config.toml, and its agent in the running registry. Signing in is the
/// caller's next step. Nothing is left behind when a step fails.
pub fn add_native_account(
    nori_home: &Path,
    provider: &'static ProviderAccounts,
    name: &str,
) -> Result<Account> {
    if let AddPlan::Unavailable(reason) = plan_add(provider) {
        anyhow::bail!(reason);
    }
    nori_config::check_account_name(name).map_err(anyhow::Error::msg)?;
    let slug = nori_config::account_slug(provider.id, name);
    anyhow::ensure!(
        super::find(&slug).is_none(),
        "{} already has an account named {name:?}",
        provider.title
    );
    let home = nori_config::default_account_home(nori_home, provider.id, name);
    let account = nori_config::AccountToml {
        provider: provider.id.to_owned(),
        name: name.to_owned(),
        home: None,
    };
    let agents = nori_config::expand_accounts(std::slice::from_ref(&account), nori_home, &[])?;
    let agent = agents
        .into_iter()
        .next()
        .context("the account did not expand to an agent")?;
    let created = !home.exists();
    std::fs::create_dir_all(&home)
        .with_context(|| format!("could not create {}", home.display()))?;
    restrict(&home);
    let registered = NoriConfigEdits::new(nori_home)
        .add_account(provider.id, name, None)
        .apply_blocking()
        .context("could not record the account in config.toml")
        .and_then(|()| crate::registry::add_custom_agent(agent));
    if let Err(error) = registered {
        if created {
            let _ = std::fs::remove_dir(&home);
        }
        return Err(error);
    }
    super::find(&slug).context("the account was added but the registry does not list it")
}

/// An account's home holds a login: nobody else needs to read it.
fn restrict(home: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = home;
}
