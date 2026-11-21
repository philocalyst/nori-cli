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

/// The variable that hands an in-process agent of `provider` the executable it starts as a
/// process, and that executable (the user's override, else the command's name). Only Claude's
/// agent runs a program (the `claude` CLI); the others, OpenCode's included, run in Nori.
pub(super) fn runs_executable(provider: &ProviderAccounts) -> Option<(&'static str, String)> {
    let (variable, command) = match provider.id {
        "claude" => ("CLAUDE_CODE_EXECUTABLE", "claude"),
        _ => return None,
    };
    let executable = std::env::var(variable)
        .ok()
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| command.to_owned());
    Some((variable, executable))
}

/// The provider's own sign-in command: its executable and arguments. `None` for Codex and
/// OpenCode, whose in-process agents sign in through ACP `authenticate`.
fn login_command(provider: &ProviderAccounts) -> Option<(&'static str, &'static [&'static str])> {
    match provider.id {
        "claude" => Some(("claude", &["auth", "login", "--claudeai"])),
        _ => None,
    }
}

/// Why `provider`'s executable cannot be run by an account of its own, when it cannot.
pub(super) fn executable_problem(provider: &ProviderAccounts) -> Option<String> {
    let (_, executable) = runs_executable(provider)?;
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
    if let Some((variable, executable)) = runs_executable(provider)
        && let Some(Unwrapped { program, env, .. }) = unwrap_executable(provider, &executable)
    {
        environment.extend(env);
        environment.insert(variable.to_owned(), program.to_string_lossy().into_owned());
    }
    environment.extend(provider.environment(home));
    Ok(environment)
}

/// The environment the provider's own, default login runs with in process. Nori picks nothing for
/// it: only the home its launcher already forces (the user's `claude` wrapper exporting
/// `CLAUDE_CONFIG_DIR`) is named, so the agent reads its settings and plans where its login
/// actually lives instead of guessing `~/.claude`.
pub fn default_environment(provider: &ProviderAccounts) -> HashMap<String, String> {
    // Only Claude's agent runs a program whose launcher may force a home.
    let Some((_, executable)) = runs_executable(provider) else {
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
    let executable = runs_executable(account.provider)
        .map_or_else(|| executable.to_owned(), |(_, executable)| executable);
    let unwrapped = unwrap_executable(account.provider, &executable)?;
    let mut env = unwrapped.env;
    env.extend(account.provider.environment(home));
    Some(LoginProgram {
        command: unwrapped.program.to_string_lossy().into_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env,
    })
}

/// The program that answers "who is this account of Claude signed in as" (`claude auth status`)
/// and the environment to run it with: where the account's own agent would run Claude, so the
/// answer is about the login that agent uses. A native account runs the unwrapped executable
/// with its home; the default account runs `claude` as the user has it (a launcher that pins a
/// profile is that profile's login); a wrapper that pins a home runs the real program with it.
pub(super) fn claude_status_program(
    account: &super::Account,
) -> Result<(String, HashMap<String, String>), String> {
    let provider = account.provider;
    let (_, executable) = runs_executable(provider)
        .ok_or_else(|| format!("{} has no program to ask", provider.title))?;
    match (&account.origin, &account.home) {
        (Origin::Default, _) | (Origin::Wrapper { .. }, Home::Default) => {
            Ok((executable, default_environment(provider)))
        }
        (Origin::Native | Origin::Wrapper { .. }, Home::At(home)) => {
            let unwrapped = unwrap_executable(provider, &executable).ok_or_else(|| {
                format!(
                    "`{executable}` is a launcher Nori cannot follow safely, or is not installed"
                )
            })?;
            let mut env = unwrapped.env;
            env.extend(provider.environment(home));
            Ok((unwrapped.program.to_string_lossy().into_owned(), env))
        }
        (Origin::Native, Home::Default) => Err("the account has no home of its own".to_owned()),
    }
}
