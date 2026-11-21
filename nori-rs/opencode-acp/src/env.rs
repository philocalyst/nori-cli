//! The environment one OpenCode agent runs in: its variables, the agent's spec's first.
//!
//! Nothing here writes to the process environment.

use std::collections::HashMap;
use std::path::PathBuf;

/// Variables: the agent's own first, then the process's.
#[derive(Debug, Clone, Default)]
pub(crate) struct Env {
    overrides: HashMap<String, String>,
}

impl Env {
    /// The agent's variables on top of the process's.
    pub(crate) fn inherited(overrides: HashMap<String, String>) -> Self {
        Self { overrides }
    }

    /// A variable's value; an empty one counts as unset.
    pub(crate) fn get(&self, key: &str) -> Option<String> {
        self.overrides
            .get(key)
            .cloned()
            .or_else(|| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
    }

    /// The user's home directory.
    pub(crate) fn home(&self) -> PathBuf {
        self.get("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Where Nori keeps this agent's own sessions: `$NORI_HOME/agents/opencode`, `NORI_HOME`
/// defaulting to `~/.nori/cli`. Shared by every account, so a session resumes under any of them.
pub(crate) fn store_root(env: &Env) -> PathBuf {
    env.get("NORI_HOME")
        .map_or_else(|| env.home().join(".nori/cli"), PathBuf::from)
        .join("agents")
        .join("opencode")
}
