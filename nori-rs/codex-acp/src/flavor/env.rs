//! The environment one Codex agent runs in: its variables and the directories they name.
//!
//! An account's variables come from the agent's spec and win over the process's, so two accounts
//! in one Nori read two Codex homes. Nothing here writes to the process environment.

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
            .filter(|value| !value.trim().is_empty())
    }

    /// The user's home directory.
    pub(crate) fn home(&self) -> PathBuf {
        self.get("HOME")
            .map(PathBuf::from)
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Codex's home: `CODEX_HOME`, else `~/.codex`.
    pub(crate) fn codex_home(&self) -> PathBuf {
        self.get("CODEX_HOME")
            .map_or_else(|| self.home().join(".codex"), PathBuf::from)
    }

    /// Where Nori keeps this agent's own sessions: `$NORI_HOME/agents/codex`, `NORI_HOME`
    /// defaulting to `~/.nori/cli`. Shared by every account, so a session resumes under any of
    /// them.
    pub(crate) fn store_root(&self) -> PathBuf {
        self.get("NORI_HOME")
            .map_or_else(|| self.home().join(".nori/cli"), PathBuf::from)
            .join("agents")
            .join("codex")
    }
}
