//! Reading an `[[agents]]` wrapper as an account of its provider.
//!
//! A wrapper is a launcher script (`nori-codex2-acp`) that points the provider at its home by
//! exporting the provider's variable, often one script further (`nori-claude2-acp` names the
//! `claude2` launcher, which exports `CLAUDE_CONFIG_DIR`). Only the launcher scripts the user
//! configured are read: small text files with a `#!` line, and only their `export` lines. A
//! binary, a large file or anything else is left alone, and no credential is ever opened.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use nori_config::PROVIDERS;
use nori_config::ProviderAccounts;

/// Launcher scripts are short; anything larger is not one.
pub(super) const MAX_SCRIPT_BYTES: u64 = 64 * 1024;
/// How many launchers deep a wrapper may name the next one.
pub(super) const MAX_HOPS: usize = 2;
/// Variables that name the executable a wrapper hands over to.
const HAND_OVER: [&str; 2] = ["CLAUDE_CODE_EXECUTABLE", "CODEX_PATH"];

pub(super) struct Scan {
    pub provider: &'static ProviderAccounts,
    /// The home the launcher forces, when it sets the provider's variable.
    pub home: Option<PathBuf>,
    /// The launcher's file name.
    pub launcher: String,
}

/// The provider a `Local` agent launches and the home it forces, when it can be told. The
/// distribution's own environment is read first, then the launcher script and the one it hands
/// over to; a provider is also recognized by the first word of the agent's slug.
pub(super) fn scan(slug: &str, command: &str, env: &HashMap<String, String>) -> Option<Scan> {
    let mut exports: HashMap<String, String> = env.clone();
    let launcher = Path::new(command).file_name().map_or_else(
        || command.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut script = resolve(command);
    for _ in 0..=MAX_HOPS {
        let Some(path) = script.take() else { break };
        let found = read_exports(&path);
        script = HAND_OVER
            .iter()
            .find_map(|name| found.get(*name))
            .and_then(|next| resolve(next));
        for (key, value) in found {
            exports.entry(key).or_insert(value);
        }
    }
    let by_variable = PROVIDERS.iter().find(|provider| {
        provider
            .home
            .is_some_and(|variable| exports.contains_key(variable.name))
    });
    let by_slug = || {
        let word: String = slug
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect::<String>()
            .to_lowercase();
        PROVIDERS.iter().find(|provider| provider.id == word)
    };
    let provider = by_variable.or_else(by_slug)?;
    let home = provider
        .home
        .and_then(|variable| exports.get(variable.name))
        .map(PathBuf::from);
    Some(Scan {
        provider,
        home,
        launcher,
    })
}

/// A command as a file: a path, or a name on `PATH`.
pub(super) fn resolve(command: &str) -> Option<PathBuf> {
    which::which(command).ok()
}

/// The plain `export NAME=value` assignments of a small script; `$HOME` and `~` are expanded,
/// anything computed (`$(...)`, backticks) is skipped.
fn read_exports(path: &Path) -> HashMap<String, String> {
    let Some(text) = read_script(path) else {
        return HashMap::new();
    };
    let user_home = dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    text.lines()
        .filter_map(|line| {
            let assignment = line.trim().strip_prefix("export ")?;
            let (name, value) = assignment.split_once('=')?;
            let value = value.trim().trim_matches(['"', '\'']);
            if name.is_empty()
                || !name
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
                || value.contains("$(")
                || value.contains('`')
            {
                return None;
            }
            let value = match &user_home {
                Some(home) => value
                    .replace("${HOME}", home)
                    .replace("$HOME", home)
                    .replacen("~/", &format!("{home}/"), 1),
                None => value.to_owned(),
            };
            Some((name.to_owned(), value))
        })
        .collect()
}

/// The text of a small `#!` script; `None` for a binary, a large file or a file that is not
/// one.
pub(super) fn read_script(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_SCRIPT_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(path).ok()?;
    text.starts_with("#!").then_some(text)
}
