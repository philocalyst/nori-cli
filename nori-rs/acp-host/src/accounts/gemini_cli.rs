//! The Gemini CLI (`gemini`), found on disk.
//!
//! Nori runs Gemini as `gemini --acp`, the CLI's own ACP agent. A user's `gemini` is wherever
//! their package manager put it, and a GUI-launched or nix-wrapped Nori often has a `PATH` that
//! does not list that place, so besides `PATH` the usual install locations are searched (the npm
//! global prefix, Homebrew, nix profiles, version managers). When none is found Nori can still
//! start the CLI through `npx` or `bunx`, if one is installed; when that is missing too, the
//! answer is "not installed", with the one command that suits this machine.
//!
//! Google moved personal accounts (Google AI Pro and Ultra, Gemini Code Assist for individuals)
//! from the Gemini CLI to Antigravity, so what the CLI is still for is a paid Gemini API key,
//! Vertex AI or a Code Assist Standard or Enterprise login ([`PERSONAL_ACCOUNTS`]).

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;

/// Where the CLI is published.
pub const NPM_PACKAGE: &str = "@google/gemini-cli";

/// What the CLI is, and is not, for now, in words a sign-in page can show.
pub const PERSONAL_ACCOUNTS: &str = "Google moved personal Google accounts (AI Pro and Ultra, free Code Assist) to Antigravity; the Gemini CLI still runs with a paid Gemini API key, Vertex AI or a Code Assist Standard or Enterprise login";

/// How an API key reaches the CLI. It is read from the environment of the process Nori starts, so
/// a key typed into Nori would only last for one connection and is never asked for.
pub const KEY_VARIABLE: &str = "GEMINI_API_KEY";

/// A `gemini` on `PATH`, else in a usual install location.
pub fn discover() -> Option<PathBuf> {
    if let Ok(path) = which::which("gemini") {
        return Some(path);
    }
    let environment = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    find_in(&usual_directories(
        environment,
        dirs::home_dir().as_deref(),
        Path::new("/"),
    ))
}

/// Whether Nori can start the CLI without it being installed: a package runner is on `PATH`.
pub fn runner_available() -> bool {
    which::which("npx").is_ok() || which::which("bunx").is_ok()
}

/// The directories a `gemini` is usually in, most specific first. `environment` reads a variable,
/// `home` is the user's home and `root` stands for `/` (so tests can use a stand-in).
fn usual_directories(
    environment: impl Fn(&str) -> Option<std::ffi::OsString>,
    home: Option<&Path>,
    root: &Path,
) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    // npm: the prefix `npm config get prefix` names, then the one people set for global installs.
    for variable in ["NPM_CONFIG_PREFIX", "npm_config_prefix"] {
        if let Some(prefix) = environment(variable) {
            found.push(PathBuf::from(prefix).join("bin"));
        }
    }
    if let Some(bun) = environment("BUN_INSTALL") {
        found.push(PathBuf::from(bun).join("bin"));
    }
    if let Some(pnpm) = environment("PNPM_HOME") {
        found.push(PathBuf::from(pnpm));
    }
    if let Some(home) = home {
        for relative in [
            ".npm-global/bin",
            ".local/bin",
            ".bun/bin",
            ".volta/bin",
            ".local/share/pnpm",
            ".local/share/mise/shims",
            ".asdf/shims",
            ".nix-profile/bin",
            ".local/state/nix/profile/bin",
            "bin",
        ] {
            found.push(home.join(relative));
        }
        // nvm keeps a prefix per node version.
        if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
            let mut versions: Vec<PathBuf> = versions
                .flatten()
                .map(|version| version.path().join("bin"))
                .collect();
            versions.sort();
            versions.reverse();
            found.extend(versions);
        }
    }
    if let Some(user) = environment("USER") {
        found.push(root.join("etc/profiles/per-user").join(user).join("bin"));
    }
    for absolute in [
        "run/current-system/sw/bin",
        "opt/homebrew/bin",
        "usr/local/bin",
        "home/linuxbrew/.linuxbrew/bin",
        "usr/bin",
    ] {
        found.push(root.join(absolute));
    }
    found
}

/// The first `gemini` that is a runnable file in `directories`.
fn find_in(directories: &[PathBuf]) -> Option<PathBuf> {
    directories
        .iter()
        .map(|directory| directory.join("gemini"))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The one command that installs the CLI on this machine: npm when there is one (the official
/// way), else Homebrew, else nix.
pub fn install_command() -> String {
    let has = |program: &str| which::which(OsStr::new(program)).is_ok();
    install_command_for(has("npm"), has("brew"), has("nix"))
}

fn install_command_for(npm: bool, brew: bool, nix: bool) -> String {
    match (npm, brew, nix) {
        (true, _, _) => format!("npm install -g {NPM_PACKAGE}"),
        (false, true, _) => "brew install gemini-cli".to_owned(),
        (false, false, true) => "nix profile install nixpkgs#gemini-cli".to_owned(),
        (false, false, false) => {
            format!("install Node.js, then run npm install -g {NPM_PACKAGE}")
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::unix::fs::PermissionsExt;

    use pretty_assertions::assert_eq;

    use super::*;

    fn install(directory: &Path) -> PathBuf {
        std::fs::create_dir_all(directory).expect("make the directory");
        let path = directory.join("gemini");
        std::fs::write(&path, "#!/bin/sh\n").expect("write gemini");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn nothing(_: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn a_gemini_in_the_npm_global_prefix_is_found_though_path_does_not_list_it() {
        let home = tempfile::tempdir().expect("tempdir");
        let installed = install(&home.path().join(".npm-global/bin"));
        let directories = usual_directories(nothing, Some(home.path()), Path::new("/nonexistent"));
        assert_eq!(find_in(&directories), Some(installed));
    }

    #[test]
    fn a_configured_npm_prefix_wins_over_the_usual_places() {
        let home = tempfile::tempdir().expect("tempdir");
        let prefix = tempfile::tempdir().expect("tempdir");
        install(&home.path().join(".npm-global/bin"));
        let configured = install(&prefix.path().join("bin"));
        let variables = HashMap::from([(
            "NPM_CONFIG_PREFIX".to_owned(),
            prefix.path().as_os_str().to_owned(),
        )]);
        let directories = usual_directories(
            |name| variables.get(name).cloned(),
            Some(home.path()),
            Path::new("/nonexistent"),
        );
        assert_eq!(find_in(&directories), Some(configured));
    }

    #[test]
    fn homebrew_nix_profiles_and_nvm_are_searched() {
        let root = tempfile::tempdir().expect("tempdir");
        let home = tempfile::tempdir().expect("tempdir");
        let brew = install(&root.path().join("opt/homebrew/bin"));
        let directories = usual_directories(nothing, Some(home.path()), root.path());
        assert_eq!(find_in(&directories), Some(brew));

        let nix = install(&root.path().join("etc/profiles/per-user/robert/bin"));
        let directories = usual_directories(
            |name| (name == "USER").then(|| OsString::from("robert")),
            Some(home.path()),
            root.path(),
        );
        assert_eq!(find_in(&directories), Some(nix));

        let nvm = install(&home.path().join(".nvm/versions/node/v22.1.0/bin"));
        let newest = install(&home.path().join(".nvm/versions/node/v24.0.0/bin"));
        let directories = usual_directories(nothing, Some(home.path()), Path::new("/nonexistent"));
        assert_eq!(find_in(&directories), Some(newest));
        assert!(directories.contains(&nvm.parent().expect("bin").to_path_buf()));
    }

    #[test]
    fn a_file_that_is_not_executable_is_not_a_gemini() {
        let home = tempfile::tempdir().expect("tempdir");
        let directory = home.path().join(".local/bin");
        std::fs::create_dir_all(&directory).expect("mkdir");
        std::fs::write(directory.join("gemini"), "not a program").expect("write");
        let directories = usual_directories(nothing, Some(home.path()), Path::new("/nonexistent"));
        assert_eq!(find_in(&directories), None);
    }

    #[test]
    fn the_install_command_suits_the_machine() {
        assert_eq!(
            install_command_for(true, true, true),
            "npm install -g @google/gemini-cli"
        );
        assert_eq!(
            install_command_for(false, true, true),
            "brew install gemini-cli"
        );
        assert_eq!(
            install_command_for(false, false, true),
            "nix profile install nixpkgs#gemini-cli"
        );
        assert_eq!(
            install_command_for(false, false, false),
            "install Node.js, then run npm install -g @google/gemini-cli"
        );
    }
}
