//! Running a provider's real executable, not the launcher script in front of it.
//!
//! The user's `claude` is a launcher: it exports `CLAUDE_CONFIG_DIR` for one profile and execs the
//! real program. An in-process account that ran that launcher would land in that profile whatever
//! home Nori set. So an account runs what the launcher runs, with the launcher's other exports,
//! and the account's own home in place of the profile's.
//!
//! A launcher is followed only when it is plain: `export NAME=value` lines and one
//! `exec <program> <args> "$@"`. Anything computed, branching or unset is not followed, and the
//! answer is `None`: Nori says why the account is unavailable rather than guess where a script
//! would land.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use nori_config::ProviderAccounts;

use super::wrapper::MAX_HOPS;
use super::wrapper::read_script;
use super::wrapper::resolve;

/// A provider's real executable and what its launchers add.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unwrapped {
    pub program: PathBuf,
    /// The arguments the launchers pass before the caller's own, innermost launcher first.
    pub args: Vec<String>,
    /// The launchers' exports, without the provider's home variable.
    pub env: HashMap<String, String>,
}

/// What one file is.
enum Step {
    /// The program itself: a binary, or a script with no `exec` line.
    Real,
    Launcher {
        exports: Vec<(String, String)>,
        target: String,
        args: Vec<String>,
    },
}

/// Follow `name_or_path` (a command on `PATH` or a path) through its launchers to the real
/// executable. `None` when a launcher cannot be followed safely, the chain is deeper than
/// [`MAX_HOPS`], or nothing is there.
pub fn unwrap_executable(provider: &ProviderAccounts, name_or_path: &str) -> Option<Unwrapped> {
    let mut current = resolve(name_or_path)?;
    let mut env: HashMap<String, String> = HashMap::new();
    let mut args: Vec<String> = Vec::new();
    for _ in 0..=MAX_HOPS {
        match follow(&current)? {
            Step::Real => {
                if let Some(variable) = provider.home {
                    env.remove(variable.name);
                }
                return Some(Unwrapped {
                    program: current,
                    args,
                    env,
                });
            }
            Step::Launcher {
                exports,
                target,
                args: inner,
            } => {
                // The launcher that runs later overrides the one before it.
                env.extend(exports);
                args.splice(0..0, inner);
                current = resolve(&target)?;
            }
        }
    }
    None
}

fn follow(path: &Path) -> Option<Step> {
    let Some(text) = read_script(path) else {
        return Some(Step::Real);
    };
    if !text
        .lines()
        .any(|line| line.trim_start().starts_with("exec "))
    {
        return Some(Step::Real);
    }
    let user_home = dirs::home_dir()?.to_string_lossy().into_owned();
    let mut exports = Vec::new();
    let mut exec: Option<(String, Vec<String>)> = None;
    for line in text.lines().skip(1).map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if exec.is_some() {
            return None;
        }
        if let Some(assignment) = line.strip_prefix("export ") {
            let (name, value) = assignment.split_once('=')?;
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                return None;
            }
            let mut words = words(value, &user_home)?;
            if words.len() != 1 {
                return None;
            }
            exports.push((name.to_owned(), words.remove(0)));
        } else if let Some(command) = line.strip_prefix("exec ") {
            let command = command.trim_end().strip_suffix("\"$@\"")?;
            let mut words = words(command, &user_home)?.into_iter();
            exec = Some((words.next()?, words.collect()));
        } else if !line.starts_with("set -") {
            // Branching, `unset`, assignments Nori cannot evaluate: not a plain launcher.
            return None;
        }
    }
    let (target, args) = exec?;
    Some(Step::Launcher {
        exports,
        target,
        args,
    })
}

/// Split a shell fragment into words, understanding quotes and `$HOME` and nothing else. `None`
/// when it holds anything that would have to be evaluated.
fn words(fragment: &str, user_home: &str) -> Option<Vec<String>> {
    let fragment = fragment
        .replace("${HOME}", user_home)
        .replace("$HOME", user_home);
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in fragment.chars() {
        match (quote, c) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), '$' | '`' | '\\')
            | (
                None,
                '$' | '`' | '\\' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '{' | '}' | '*' | '?'
                | '~',
            ) => {
                return None;
            }
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}
