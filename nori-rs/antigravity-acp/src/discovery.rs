//! Where the `localharness` the SDK drives lives.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

use nori_acp_kit::AgentError;

/// How to reach the harness, as configured. Explicit settings win over the per-agent
/// environment, which wins over the process environment.
#[derive(Debug, Clone, Default)]
pub(crate) struct Backend {
    pub(crate) harness_path: Option<PathBuf>,
    pub(crate) ws_url: Option<String>,
    /// Per-agent environment, forwarded to the harness process.
    pub(crate) environment: HashMap<String, String>,
}

/// Resolved harness connection target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HarnessEndpoint {
    /// Connect directly to an already-running `localharness` WebSocket URL.
    WsUrl(String),
    /// Spawn a local `localharness` subprocess at this binary path.
    Binary(PathBuf),
}

impl Backend {
    fn setting(&self, key: &str) -> Option<String> {
        self.environment
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    /// Whether `key` is set, without reading its value anywhere it could be shown.
    pub(crate) fn has_setting(&self, key: &str) -> bool {
        self.setting(key).is_some()
    }

    /// Discover the endpoint from settings, environment, `./bin`, `PATH`, or the Python wheel.
    pub(crate) fn resolve_endpoint(&self) -> Result<HarnessEndpoint, AgentError> {
        if let Some(url) = self
            .ws_url
            .clone()
            .filter(|url| !url.trim().is_empty())
            .or_else(|| self.setting("ANTIGRAVITY_WS_URL"))
        {
            return Ok(HarnessEndpoint::WsUrl(url.trim().to_owned()));
        }
        if let Some(path) = self
            .harness_path
            .clone()
            .or_else(|| self.setting("ANTIGRAVITY_HARNESS_PATH").map(PathBuf::from))
            .filter(|path| path.exists())
        {
            return Ok(HarnessEndpoint::Binary(path));
        }

        let binary_name = if cfg!(target_os = "windows") {
            "localharness.exe"
        } else {
            "localharness"
        };
        let in_cwd = std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join("bin").join(binary_name));
        let on_path = self
            .setting("PATH")
            .map(std::ffi::OsString::from)
            .into_iter()
            .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .map(|dir| dir.join(binary_name));
        if let Some(found) = in_cwd
            .into_iter()
            .chain(on_path)
            .find(|candidate| candidate.exists())
            .or_else(|| python_wheel_binary(binary_name))
        {
            return Ok(HarnessEndpoint::Binary(found));
        }
        Err(AgentError::Internal(
            "Could not find the Antigravity `localharness` binary, which the `antigravity-sdk` \
             agent drives. Set ANTIGRAVITY_HARNESS_PATH (or `harness_path=` in the agent's native \
             config), point `ws_url=` at a running harness, or install the `google-antigravity` \
             Python package. The default `antigravity` agent does not need it: it runs the \
             signed-in official `agy` CLI."
                .to_owned(),
        ))
    }
}

fn python_wheel_binary(binary_name: &str) -> Option<PathBuf> {
    let script = concat!(
        "import importlib.util, pathlib, site\n",
        "paths = list(site.getsitepackages())\n",
        "spec = importlib.util.find_spec('google.antigravity')\n",
        "if spec and spec.submodule_search_locations:\n",
        "    paths.extend(str(pathlib.Path(p).parent.parent) for p in spec.submodule_search_locations)\n",
        "print('\\n'.join(paths))\n",
    );
    let output = std::process::Command::new("python3")
        .args(["-c", script])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            Path::new(line)
                .join("google")
                .join("antigravity")
                .join("bin")
                .join(binary_name)
        })
        .find(|candidate| candidate.exists())
}
