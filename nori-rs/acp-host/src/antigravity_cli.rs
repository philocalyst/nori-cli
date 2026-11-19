//! The official Antigravity CLI (`agy`), found on disk and checked for ACP support.
//!
//! From 1.5.10 the signed-in `agy` speaks ACP itself (`agy acp`), so Nori starts it like any other
//! ACP agent and reads its capabilities from its own `initialize` response. This module only finds
//! the newest installed `agy` and refuses a version that has no `acp` subcommand, with a message
//! that says what is installed and how to upgrade. It never reads the CLI's credentials.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::PoisonError;
use std::time::Duration;
use std::time::SystemTime;

use anyhow::Result;

/// The first `agy` release with `agy acp`.
pub const MINIMUM_VERSION: CliVersion = CliVersion {
    major: 1,
    minor: 5,
    patch: 10,
};

/// What to run to upgrade the official CLI.
pub const UPGRADE_COMMAND: &str = "agy update";

/// What to run to sign in, when the CLI says authentication is required.
pub const LOGIN_COMMAND: &str = "agy acp --login";

/// The arguments Nori starts `agy` with. Every session gets its own worker process, which is how
/// `agy acp` serves the HTTP MCP servers Nori hands its agents.
pub const ACP_ARGS: [&str; 2] = ["acp", "--process-per-session"];

/// How long `agy --version` may take before the CLI is considered broken.
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// A release number, ordered the way releases are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CliVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
}

impl CliVersion {
    /// The first `major.minor.patch` in `text`, such as the output of `agy --version` or a
    /// directory name.
    pub fn parse(text: &str) -> Option<Self> {
        text.split(|character: char| !character.is_ascii_digit() && character != '.')
            .find_map(|word| {
                let mut parts = word.split('.').map(|part| part.parse::<i32>().ok());
                Some(Self {
                    major: parts.next()??,
                    minor: parts.next()??,
                    patch: parts.next()??,
                })
            })
    }
}

impl std::fmt::Display for CliVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// An `agy` that can serve ACP.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyInstall {
    pub path: PathBuf,
    pub version: CliVersion,
}

/// Where Nori keeps `agy` releases it installed side by side: `<dir>/<version>/agy`.
fn managed_dir() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".local").join("share")))?;
    Some(data.join("nori").join("antigravity-cli"))
}

/// Every `agy` on `PATH`, then every release under [`managed_dir`], newest directory first.
fn candidates() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = which::which_all("agy").into_iter().flatten().collect();
    if let Some(entries) = managed_dir().and_then(|dir| std::fs::read_dir(dir).ok()) {
        let mut releases: Vec<(CliVersion, PathBuf)> = entries
            .flatten()
            .filter_map(|entry| {
                let version = CliVersion::parse(&entry.file_name().to_string_lossy())?;
                let binary = entry.path().join("agy");
                binary.is_file().then_some((version, binary))
            })
            .collect();
        releases.sort_by_key(|release| std::cmp::Reverse(release.0));
        found.extend(releases.into_iter().map(|(_, binary)| binary));
    }
    let mut unique: Vec<PathBuf> = Vec::with_capacity(found.len());
    for path in found {
        let path = path.canonicalize().unwrap_or(path);
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// Whether an `agy` of any version can be found. Cheap: it runs nothing.
pub fn is_installed() -> bool {
    !candidates().is_empty()
}

fn version_cache() -> &'static Mutex<HashMap<PathBuf, (SystemTime, CliVersion)>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, CliVersion)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// What `path --version` reports. Cached until the file changes, so an `agy update` is noticed.
fn installed_version(path: &Path) -> Result<CliVersion> {
    let modified = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    if let Some((cached_at, version)) = version_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        && *cached_at == modified
    {
        return Ok(*version);
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    let command_path = path.to_path_buf();
    std::thread::spawn(move || {
        let output = std::process::Command::new(&command_path)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        let _ = sender.send(output);
    });
    let output = receiver
        .recv_timeout(VERSION_TIMEOUT)
        .map_err(|_| {
            anyhow::anyhow!(
                "`{} --version` did not answer within {} seconds",
                path.display(),
                VERSION_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| {
            anyhow::anyhow!("could not run `{} --version`: {error}", path.display())
        })?;
    let text = String::from_utf8_lossy(&output.stdout);
    let version = CliVersion::parse(&text).ok_or_else(|| {
        anyhow::anyhow!(
            "`{} --version` printed {:?}, which is not a version number",
            path.display(),
            text.trim()
        )
    })?;
    version_cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(path.to_path_buf(), (modified, version));
    Ok(version)
}

/// The newest installed `agy` that supports `agy acp`.
///
/// Fails with what to do next: nothing installed, every installed release too old (naming the
/// newest one and the upgrade command), or no installed release answering `--version`.
pub fn discover() -> Result<AgyInstall> {
    let candidates = candidates();
    if candidates.is_empty() {
        let managed = managed_dir().map_or_else(
            || "~/.local/share/nori/antigravity-cli".to_owned(),
            |dir| dir.display().to_string(),
        );
        anyhow::bail!(
            "The Antigravity CLI (`agy`) is not installed. Install the official `agy` \
             {MINIMUM_VERSION} or newer so that it is on PATH (or at {managed}/<version>/agy), \
             then sign in with `{LOGIN_COMMAND}`."
        );
    }
    let mut newest: Option<AgyInstall> = None;
    let mut failures = Vec::new();
    for path in candidates {
        match installed_version(&path) {
            Ok(version) => {
                if newest.as_ref().is_none_or(|best| version > best.version) {
                    newest = Some(AgyInstall { path, version });
                }
            }
            Err(error) => failures.push(error.to_string()),
        }
    }
    let Some(install) = newest else {
        anyhow::bail!(
            "The Antigravity CLI is installed but does not run: {}",
            failures.join("; ")
        );
    };
    if install.version < MINIMUM_VERSION {
        anyhow::bail!(
            "The installed Antigravity CLI is {} (at {}), which has no ACP support. Nori needs \
             {MINIMUM_VERSION} or newer: upgrade with `{UPGRADE_COMMAND}`.",
            install.version,
            install.path.display()
        );
    }
    Ok(install)
}
