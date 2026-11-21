//! What Nori knows about a machine from probing it, and where it keeps that between runs.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

use crate::MachineName;

/// The machine as its probe saw it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Facts {
    /// `uname -s`: `Darwin`, `Linux`, ...
    pub os: String,
    /// `uname -m`: `arm64`, `x86_64`, ...
    pub arch: String,
    /// `uname -r`.
    pub release: String,
    pub hostname: String,
    pub home: String,
    /// The login shell.
    pub shell: String,
    /// Where bash is, when it is installed.
    pub bash: Option<String>,
    /// The login shell's `PATH`.
    pub path: Option<String>,
    /// When the probe ran, in seconds since the Unix epoch.
    pub probed_at: i64,
}

impl Facts {
    /// Parse what [`crate::script::PROBE`] printed.
    pub(crate) fn parse(stdout: &str, probed_at: i64) -> Option<Self> {
        let mut lines = stdout.lines();
        if lines.next()?.trim() != "nori-probe 1" {
            return None;
        }
        let fields: BTreeMap<&str, &str> = lines.filter_map(|line| line.split_once('=')).collect();
        let field = |key: &str| fields.get(key).map(|value| value.trim().to_owned());
        let optional = |key: &str| field(key).filter(|value| !value.is_empty());
        Some(Self {
            os: field("os").unwrap_or_default(),
            arch: field("arch").unwrap_or_default(),
            release: field("release").unwrap_or_default(),
            hostname: field("hostname").unwrap_or_default(),
            home: field("home").unwrap_or_default(),
            shell: field("shell").unwrap_or_default(),
            bash: optional("bash"),
            path: optional("path"),
            probed_at,
        })
    }

    /// `macOS arm64`, `Linux x86_64`: what the agent needs to pick commands.
    pub fn platform(&self) -> String {
        let os = match self.os.as_str() {
            "Darwin" => "macOS",
            "" => "unknown OS",
            other => other,
        };
        if self.arch.is_empty() {
            os.to_owned()
        } else {
            format!("{os} {}", self.arch)
        }
    }
}

/// The facts of every machine probed, kept in `machines.json` in Nori's home so the next run
/// can describe machines without connecting to them first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FactsCache {
    #[serde(default)]
    pub(crate) machines: BTreeMap<MachineName, Facts>,
    /// When a machine last did not answer, in seconds since the Unix epoch, and why: a machine
    /// that refused a moment ago is not asked again at every start (sshd punishes a stream of
    /// failed connections from one source).
    #[serde(default)]
    pub(crate) unreachable: BTreeMap<MachineName, (i64, String)>,
}

impl FactsCache {
    pub(crate) fn path(nori_home: &Path) -> PathBuf {
        nori_home.join("machines.json")
    }

    pub(crate) fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Write atomically: a crash mid-write leaves the old file.
    pub(crate) fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, text)?;
        std::fs::rename(temporary, path)
    }
}

pub(crate) fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_probe_reads_back_as_facts() {
        let stdout = "nori-probe 1\nos=Darwin\narch=arm64\nrelease=25.6.0\nhostname=h16003mac\nhome=/Users/me\nshell=/bin/zsh\nbash=\npath=/opt/homebrew/bin:/usr/bin:/bin\n";
        let facts = Facts::parse(stdout, 7);
        assert_eq!(
            facts,
            Some(Facts {
                os: "Darwin".to_owned(),
                arch: "arm64".to_owned(),
                release: "25.6.0".to_owned(),
                hostname: "h16003mac".to_owned(),
                home: "/Users/me".to_owned(),
                shell: "/bin/zsh".to_owned(),
                bash: None,
                path: Some("/opt/homebrew/bin:/usr/bin:/bin".to_owned()),
                probed_at: 7,
            })
        );
        assert_eq!(
            facts.map(|facts| facts.platform()),
            Some("macOS arm64".to_owned())
        );
        assert_eq!(Facts::parse("Welcome!\nos=Linux", 0), None);
    }
}
