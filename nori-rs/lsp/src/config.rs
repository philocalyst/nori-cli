//! Settings of the language-server manager. Everything has a default; nothing reads
//! `nori-config` yet: the host builds an [`LspConfig`] from its own settings.

use std::time::Duration;

use crate::registry::ServerConfig;

/// Every wait the layer makes, so a host (or a test) can tighten them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timing {
    /// How long `diagnostics_after_edit` waits for the first publish after a change.
    pub first_diagnostics: Duration,
    /// Silence after the first publish that counts as "the server is done".
    pub settle: Duration,
    /// Upper bound on the settle phase, for servers that never stop publishing.
    pub settle_cap: Duration,
    /// The `initialize` handshake.
    pub init: Duration,
    /// One request to a ready server.
    pub request: Duration,
    /// How long a call waits for a server that is still starting.
    pub ready_wait: Duration,
    /// Graceful `shutdown` before the process is killed.
    pub shutdown: Duration,
    /// First restart delay; it doubles per failure up to `backoff_cap`.
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            first_diagnostics: Duration::from_millis(1000),
            settle: Duration::from_millis(300),
            settle_cap: Duration::from_secs(3),
            init: Duration::from_secs(30),
            request: Duration::from_secs(20),
            ready_wait: Duration::from_secs(6),
            shutdown: Duration::from_secs(2),
            backoff_base: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LspConfig {
    /// Master switch: a disabled manager answers every call with "no language server".
    pub enabled: bool,
    /// The user's servers; they replace built-ins of the same name and win within a language.
    pub servers: Vec<ServerConfig>,
    pub timing: Timing,
    /// A server that crashes is restarted lazily on the next call at most this many times.
    pub max_restarts: i32,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            servers: Vec::new(),
            timing: Timing::default(),
            max_restarts: 3,
        }
    }
}
