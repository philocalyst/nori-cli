//! What a native account runs: its provider's real executable with the account's home.

use std::collections::HashMap;
use std::path::Path;

use nori_config::ProviderAccounts;

use super::Home;
use super::Origin;
use super::unwrap::Unwrapped;
use super::unwrap::unwrap_executable;

/// A sign-in program to run where the user can answer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginProgram {
    pub command: String,
    pub args: Vec<String>,
    /// The account's home on top of what the real executable's launchers export.
    pub env: HashMap<String, String>,
}

/// The executable an in-process agent of `provider` starts as a process, if it starts one. Claude's
/// agent drives the `claude` CLI; the others run their engine, or talk to a server, in Nori.
fn runs_executable(provider: &ProviderAccounts) -> Option<String> {
    match provider.id {
        "claude" => Some(
            std::env::var("CLAUDE_CODE_EXECUTABLE")
                .ok()
                .filter(|path| !path.is_empty())
                .unwrap_or_else(|| "claude".to_owned()),
        ),
        _ => None,
    }
}

/// The provider's own sign-in command: its executable and arguments. `None` for Codex, whose
/// in-process agent signs in through ACP `authenticate`.
fn login_command(provider: &ProviderAccounts) -> Option<(&'static str, &'static [&'static str])> {
    match provider.id {
        "claude" => Some(("claude", &["auth", "login", "--claudeai"])),
        "gemini" => Some(("gemini", &["login"])),
        "opencode" => Some(("opencode", &["auth", "login"])),
        _ => None,
    }
}

/// Why `provider`'s executable cannot be run by an account of its own, when it cannot.
pub(super) fn executable_problem(provider: &ProviderAccounts) -> Option<String> {
    let executable = runs_executable(provider)?;
    match unwrap_executable(provider, &executable) {
        Some(_) => None,
        None => Some(format!(
            "{title} runs `{executable}`, which is a launcher Nori cannot follow safely (it must be \
             plain `export` lines and one `exec <program> ... \"$@\"`), or is not installed. An \
             account run through it could land in the wrong profile, so Nori will not add one",
            title = provider.title
        )),
    }
}

/// The environment an in-process account of `provider` at `home` starts with: the provider's home
/// variable, and for a provider that runs an executable, that executable unwrapped (its path as
/// `CLAUDE_CODE_EXECUTABLE`) with its launchers' other exports. The process environment never
/// decides the account.
pub fn native_environment(
    provider: &ProviderAccounts,
    home: &Path,
) -> Result<HashMap<String, String>, String> {
    let mut environment = HashMap::new();
    if let Some(problem) = executable_problem(provider) {
        return Err(problem);
    }
    if let Some(executable) = runs_executable(provider)
        && let Some(Unwrapped { program, env, .. }) = unwrap_executable(provider, &executable)
    {
        environment.extend(env);
        environment.insert(
            "CLAUDE_CODE_EXECUTABLE".to_owned(),
            program.to_string_lossy().into_owned(),
        );
    }
    environment.extend(provider.environment(home));
    Ok(environment)
}

/// The environment the provider's own, default login runs with in process. Nori picks nothing for
/// it: only the home its launcher already forces (the user's `claude` wrapper exporting
/// `CLAUDE_CONFIG_DIR`) is named, so the agent reads its settings and plans where its login
/// actually lives instead of guessing `~/.claude`.
pub fn default_environment(provider: &ProviderAccounts) -> HashMap<String, String> {
    let Some(executable) = runs_executable(provider) else {
        return HashMap::new();
    };
    super::wrapper::scan(provider.id, &executable, &HashMap::new())
        .filter(|scan| scan.provider.id == provider.id)
        .and_then(|scan| scan.home)
        .map(|home| provider.environment(&home))
        .unwrap_or_default()
}

/// The sign-in program of the native account `slug`: the provider's login CLI, run through its
/// unwrapped executable with the account's home. Launcher arguments are not carried over, since
/// they are written for running the agent, not for signing in. `None` for an account that is not
/// native, a provider that signs in through ACP, or an executable that cannot be unwrapped.
pub fn login_program(slug: &str) -> Option<LoginProgram> {
    let account = super::find(slug)?;
    let (Origin::Native, Home::At(home)) = (&account.origin, &account.home) else {
        return None;
    };
    let (executable, args) = login_command(account.provider)?;
    let executable = runs_executable(account.provider).unwrap_or_else(|| executable.to_owned());
    let unwrapped = unwrap_executable(account.provider, &executable)?;
    let mut env = unwrapped.env;
    env.extend(account.provider.environment(home));
    Some(LoginProgram {
        command: unwrapped.program.to_string_lossy().into_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env,
    })
}
