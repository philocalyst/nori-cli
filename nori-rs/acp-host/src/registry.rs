//! ACP agent registry
//!
//! Provides configuration for ACP agents (subprocess command and args)
//! with embedded provider info to avoid circular dependencies with core.
//!
//! ## Agent Names
//! - `claude-code` - Anthropic's Claude Code CLI agent
//! - `codex` - OpenAI's Codex CLI agent
//! - `gemini` - Google's Gemini CLI agent
//!
//! ## Provider Names
//! - `anthropic` - Anthropic
//! - `openai` - OpenAI
//! - `google` - Google

use anyhow::Context;
use anyhow::Result;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;

use nori_config::AgentConfigToml;
use nori_config::ResolvedDistribution;

/// Default idle timeout for ACP streaming (5 minutes)
const DEFAULT_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

fn codex_config_for_nori(config: Option<&str>) -> Result<String> {
    let mut config = match config {
        Some(config) => serde_json::from_str::<serde_json::Value>(config)
            .context("CODEX_CONFIG must be valid JSON")?,
        None => serde_json::json!({}),
    };
    let config = config
        .as_object_mut()
        .context("CODEX_CONFIG must contain a JSON object")?;
    config.insert("approval_policy".to_owned(), "never".into());
    config.insert("sandbox_mode".to_owned(), "danger-full-access".into());
    // These are alternative permission selectors, not independent account settings.
    config.remove("default_permissions");
    config.remove("permission_profile");
    let features = config
        .entry("features")
        .or_insert_with(|| serde_json::json!({}))
        .as_object_mut()
        .context("CODEX_CONFIG features must contain a JSON object")?;
    features.insert("goals".to_string(), false.into());

    serde_json::to_string(config).context("failed to serialize CODEX_CONFIG")
}

/// Merge a `model` key into a `CODEX_CONFIG` JSON object, preserving any other
/// keys already present (e.g. the goals-disabled flag).
fn codex_config_with_model(config: Option<&str>, model: &str) -> Result<String> {
    let mut value = match config {
        Some(config) => serde_json::from_str::<serde_json::Value>(config)
            .context("CODEX_CONFIG must be valid JSON")?,
        None => serde_json::json!({}),
    };
    value
        .as_object_mut()
        .context("CODEX_CONFIG must contain a JSON object")?
        .insert("model".to_string(), model.into());
    serde_json::to_string(&value).context("failed to serialize CODEX_CONFIG")
}

/// How a chosen model id is delivered to an agent subprocess at spawn time.
///
/// Runtime model changes go over the live ACP `session/set_config_option` RPC,
/// but adapters reject models outside their advertised catalog. To run such a
/// model, nori forces it through the agent's own out-of-band channel at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelInjection {
    /// Set an environment variable to the model id.
    Env { var: String },
    /// Append a CLI flag followed by the model id.
    Arg { flag: String },
    /// Merge the model id into the `CODEX_CONFIG` JSON object.
    CodexConfig,
    /// No known channel; the model can only be set via the live ACP RPC.
    None,
}

impl ModelInjection {
    /// Resolve a custom agent's declared model-override config into a strategy.
    /// `env` wins if both `env` and `arg` are set.
    fn from_override(over: &nori_config::ModelOverrideToml) -> Self {
        if let Some(var) = &over.env {
            ModelInjection::Env { var: var.clone() }
        } else if let Some(flag) = &over.arg {
            ModelInjection::Arg { flag: flag.clone() }
        } else {
            ModelInjection::None
        }
    }
}

mod kind;
pub use kind::AgentKind;
pub use kind::OtherModel;
pub use kind::PackageManager;
pub use kind::Provider;

// =============================================================================
// Data-driven Registry
// =============================================================================

/// A registered agent entry in the data-driven registry.
///
/// Built-in agents have `kind` set to `Some(AgentKind)` for first-class metadata.
/// Custom agents (from config) have `kind` set to `None`.
#[derive(Debug, Clone)]
pub struct RegisteredAgent {
    /// Display name shown in the agent picker (e.g. "Claude Code")
    pub name: String,
    /// Machine identifier / slug (e.g. "claude-code")
    pub slug: String,
    /// First-class agent kind, if this is a built-in agent
    pub kind: Option<AgentKind>,
    /// How this agent is invoked (None for built-in agents that use auto-detection)
    pub distribution: Option<ResolvedDistribution>,
    /// Context window size override (in tokens)
    pub context_window_size: Option<i64>,
    /// Auth hint displayed on auth failures
    pub auth_hint: Option<String>,
    /// Transcript base directory (relative to home)
    pub transcript_base_dir: Option<String>,
    /// How to force a model on this agent at spawn time (custom agents only;
    /// built-in agents derive this from their `AgentKind`).
    pub model_injection: ModelInjection,
}

/// Global agent registry. Protected by RwLock so tests can reset it.
static AGENT_REGISTRY: RwLock<Option<Vec<RegisteredAgent>>> = RwLock::new(None);

/// Whether the registry lists the built-in agents (Claude, Codex, ...). A world of fake agents
/// turns this off so nothing real is offered, probed or warmed (`nori-fake-agent`).
static BUILTIN_AGENTS_LISTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

/// List the built-in agents in the registry (the default), or only the agents the config
/// defines. Takes effect at the next [`initialize_registry`] and for an uninitialized registry.
pub fn list_builtin_agents(listed: bool) {
    BUILTIN_AGENTS_LISTED.store(listed, std::sync::atomic::Ordering::SeqCst);
}

/// Build the default built-in agents.
pub fn build_default_agents() -> Vec<RegisteredAgent> {
    if !BUILTIN_AGENTS_LISTED.load(std::sync::atomic::Ordering::SeqCst) {
        return Vec::new();
    }
    AgentKind::all()
        .iter()
        .map(|kind| RegisteredAgent {
            name: kind.display_name().to_string(),
            slug: kind.slug().to_string(),
            kind: Some(*kind),
            distribution: None, // built-ins use auto-detection
            context_window_size: Some(kind.context_window_size()),
            auth_hint: Some(kind.auth_hint().to_string()),
            transcript_base_dir: Some(kind.transcript_base_dir().to_string()),
            model_injection: kind.model_injection(),
        })
        .collect()
}

/// Build the full registry from built-in defaults + custom agent configs.
///
/// Custom agents with a slug matching a built-in override the built-in entry.
/// Duplicate slugs among custom agents are rejected.
pub fn build_registry(custom_agents: Vec<AgentConfigToml>) -> Result<Vec<RegisteredAgent>> {
    let mut agents = build_default_agents();

    // Check for duplicate slugs among custom agents
    let mut seen_custom_slugs = HashSet::new();
    for custom in &custom_agents {
        if !seen_custom_slugs.insert(custom.slug.clone()) {
            anyhow::bail!(
                "Duplicate custom agent slug: '{}'. Each agent must have a unique slug.",
                custom.slug
            );
        }
    }

    for custom in custom_agents {
        let resolved = custom.distribution.resolve().map_err(|e| {
            anyhow::anyhow!("Invalid distribution for agent '{}': {e}", custom.slug)
        })?;

        let registered = RegisteredAgent {
            name: custom.name,
            slug: custom.slug.clone(),
            kind: None,
            distribution: Some(resolved),
            context_window_size: custom.context_window_size,
            auth_hint: custom.auth_hint,
            transcript_base_dir: custom.transcript_base_dir,
            model_injection: custom
                .model_override
                .as_ref()
                .map(ModelInjection::from_override)
                .unwrap_or(ModelInjection::None),
        };

        // Override built-in if slug matches, otherwise append
        if let Some(existing) = agents.iter_mut().find(|a| a.slug == registered.slug) {
            *existing = registered;
        } else {
            agents.push(registered);
        }
    }

    Ok(agents)
}

/// Initialize the global agent registry with custom agents from config.
///
/// This should be called once at startup after loading config.
/// If called multiple times, later calls replace the registry.
pub fn initialize_registry(custom_agents: Vec<AgentConfigToml>) -> Result<()> {
    let agents = build_registry(custom_agents)?;
    let mut registry = AGENT_REGISTRY
        .write()
        .map_err(|e| anyhow::anyhow!("agent registry lock poisoned: {e}"))?;
    *registry = Some(agents);
    Ok(())
}

/// Add one custom agent (an account created while Nori runs) to the registry, replacing the
/// entry with the same slug.
pub fn add_custom_agent(custom: AgentConfigToml) -> Result<()> {
    let slug = custom.slug.clone();
    let agent = build_registry(vec![custom])?
        .into_iter()
        .find(|agent| agent.slug == slug)
        .with_context(|| format!("agent {slug:?} did not register"))?;
    let mut registry = AGENT_REGISTRY
        .write()
        .map_err(|e| anyhow::anyhow!("agent registry lock poisoned: {e}"))?;
    let agents = registry.get_or_insert_with(build_default_agents);
    match agents.iter_mut().find(|known| known.slug == slug) {
        Some(known) => *known = agent,
        None => agents.push(agent),
    }
    Ok(())
}

/// Every registered agent: the built-ins, then the configured ones.
pub(crate) fn registered_agents() -> Vec<RegisteredAgent> {
    get_registry()
}

/// Get the current registry, falling back to defaults if not initialized.
fn get_registry() -> Vec<RegisteredAgent> {
    match AGENT_REGISTRY.read() {
        Ok(guard) => match &*guard {
            Some(agents) => agents.clone(),
            None => build_default_agents(),
        },
        Err(_) => build_default_agents(),
    }
}

// =============================================================================
// Agent Info (for UI)
// =============================================================================

/// Information about an available ACP agent for display in the picker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAgentInfo {
    /// Agent identifier
    pub agent: AgentKind,
    /// Agent name used to select this agent (e.g., "claude-code", "gemini")
    pub agent_name: String,
    /// Display name shown in the picker
    pub display_name: String,
    /// Description of the agent
    pub description: String,
    /// Provider slug for this agent
    pub provider_slug: String,
    /// Whether the agent is currently installed
    pub is_installed: bool,
    /// Package manager used to manage this agent (if installed)
    pub managed_by: Option<PackageManager>,
    /// Why a built-in agent that runs inside Nori cannot be used right now, such as its server
    /// not running. Such an agent is not `is_installed`, but it is not missing either.
    pub unavailable: Option<String>,
}

impl AcpAgentInfo {
    /// Create agent info from an AgentKind enum
    pub fn from_agent(agent: AgentKind) -> Self {
        // A built-in agent that runs inside Nori has no CLI to find, so it counts as installed.
        let native =
            native_name(agent).filter(|name| crate::in_process::in_process_available(name));
        let unavailable = native.and_then(crate::in_process::in_process_unavailable);
        let (is_installed, managed_by) = if native.is_some() {
            (unavailable.is_none(), None)
        } else {
            detect_agent_installation(agent)
        };

        Self {
            agent,
            agent_name: agent.slug().to_string(),
            display_name: agent.display_name().to_string(),
            description: agent.provider().display_name().to_string(),
            provider_slug: agent.slug().to_string(),
            is_installed,
            managed_by,
            unavailable,
        }
    }
}

// =============================================================================
// Installation Detection
// =============================================================================

/// Cache for agent installation detection (PATH-based, fast).
/// Caching avoids repeated subprocess calls on every picker open.
static INSTALLATION_CACHE: OnceLock<HashMap<AgentKind, (bool, Option<PackageManager>)>> =
    OnceLock::new();

/// Pre-warm the installation detection cache.
///
/// Detection is now fast (PATH lookup only), so prewarming is optional.
/// Call this at app startup to avoid any startup latency from `which` commands.
pub fn prewarm_installation_cache() {
    let _ = INSTALLATION_CACHE.get_or_init(|| {
        let mut map = HashMap::new();
        for kind in AgentKind::all() {
            map.insert(*kind, detect_agent_installation_uncached(*kind));
        }
        map
    });
}

/// Detect if an agent is installed and which package manager manages it.
///
/// Uses PATH-based detection (fast `which` command) with caching.
/// Agents not in PATH can still be launched via npx/bunx.
fn detect_agent_installation(agent: AgentKind) -> (bool, Option<PackageManager>) {
    let cache = INSTALLATION_CACHE.get_or_init(|| {
        let mut map = HashMap::new();
        for kind in AgentKind::all() {
            map.insert(*kind, detect_agent_installation_uncached(*kind));
        }
        map
    });

    cache.get(&agent).copied().unwrap_or((false, None))
}

/// Perform the actual installation detection (uncached).
///
/// Only checks PATH for fast detection. Agents not in PATH can still be
/// launched via npx/bunx (which downloads and runs on-the-fly if needed).
fn detect_agent_installation_uncached(agent: AgentKind) -> (bool, Option<PackageManager>) {
    let binary_name = match agent {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::Gemini => "gemini",
        AgentKind::OpenCode => "opencode",
        // The official CLI, on PATH or in Nori's side-by-side install directory.
        AgentKind::Antigravity => return (crate::antigravity_cli::is_installed(), None),
    };

    // Look the binary up in PATH in-process: a `which` process per agent would hold up everything
    // that waits on this cache, the first frame included.
    if let Ok(path) = which::which(binary_name) {
        let managed_by = detect_package_manager_from_path(&path.to_string_lossy());
        return (true, managed_by);
    }

    // Binary not in PATH - agent is not locally installed.
    // It can still be launched via npx/bunx which will download on-the-fly.
    (false, None)
}

/// Detect package manager from binary path
fn detect_package_manager_from_path(path: &str) -> Option<PackageManager> {
    let path_lower = path.to_lowercase();
    if path_lower.contains(".bun") || path_lower.contains("bun/bin") {
        Some(PackageManager::Bun)
    } else if path_lower.contains("npm")
        || path_lower.contains("node_modules")
        || path_lower.contains(".npm")
    {
        Some(PackageManager::Npm)
    } else {
        // Default to npm if we can't determine
        Some(PackageManager::Npm)
    }
}

/// Detect the preferred package manager for launching ACP agents.
///
/// Priority:
/// 1. NORI_MANAGED_BY_BUN env var → use bunx
/// 2. NORI_MANAGED_BY_NPM env var → use npx
/// 3. bun in PATH → use bunx
/// 4. Default to npx
pub fn detect_preferred_package_manager() -> PackageManager {
    // Check explicit env var overrides first
    if std::env::var("NORI_MANAGED_BY_BUN").is_ok() {
        return PackageManager::Bun;
    }
    if std::env::var("NORI_MANAGED_BY_NPM").is_ok() {
        return PackageManager::Npm;
    }

    // Check if bun is available in PATH, without starting it.
    if which::which("bun").is_ok() {
        return PackageManager::Bun;
    }

    // Default to npm
    PackageManager::Npm
}

// =============================================================================
// Agent Config (for spawning)
// =============================================================================

/// Provider information embedded in ACP agent config.
/// This mirrors relevant fields from `ModelProviderInfo` to avoid circular dependencies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpProviderInfo {
    /// Friendly display name (e.g., "Gemini ACP", "Mock ACP")
    pub name: String,
    /// Maximum number of request retries
    pub request_max_retries: u64,
    /// Maximum number of stream reconnection attempts
    pub stream_max_retries: u64,
    /// Idle timeout for streaming responses
    pub stream_idle_timeout: Duration,
}

impl Default for AcpProviderInfo {
    fn default() -> Self {
        Self {
            name: "ACP".to_string(),
            request_max_retries: 1,
            stream_max_retries: 1,
            stream_idle_timeout: DEFAULT_STREAM_IDLE_TIMEOUT,
        }
    }
}

/// Configuration for an ACP agent subprocess
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpAgentConfig {
    /// Agent identifier (placeholder for custom agents)
    pub agent: AgentKind,
    /// Provider identifier (e.g., "claude-code", "gemini")
    /// Used to determine when subprocess can be reused vs needs replacement
    pub provider_slug: String,
    /// Command to execute (binary path or command name)
    pub command: String,
    /// Arguments to pass to the command
    pub args: Vec<String>,
    /// Environment variables to set for the subprocess
    pub env: HashMap<String, String>,
    /// Provider information for this ACP agent
    pub provider_info: AcpProviderInfo,
    /// Authentication hint for this agent (displayed on auth failures)
    pub auth_hint: String,
    /// Display name for this agent (avoids going through AgentKind for custom agents)
    pub display_name: String,
    /// Install command hint for error messages (e.g. "npm install -g @pkg" or "ensure /usr/bin/x is in PATH")
    pub install_hint: String,
    /// How to force a model on this agent at spawn time.
    pub model_injection: ModelInjection,
    /// Advertise `clientCapabilities.subagents` in `initialize`, so adapters that implement the
    /// subagents proposal report their child sessions instead of flattening or hiding them.
    pub advertise_subagents: bool,
    /// Run the agent inside Nori's own process instead of starting `command`.
    pub in_process: Option<crate::in_process::InProcessSpec>,
}

impl AcpAgentConfig {
    /// Force `model` into this agent's spawn configuration via its out-of-band
    /// channel (env var, CLI flag, or `CODEX_CONFIG` merge). A no-op for agents
    /// with no known channel, so callers may apply it unconditionally.
    pub fn inject_model(&mut self, model: &str) -> Result<()> {
        match &self.model_injection {
            ModelInjection::Env { var } => {
                self.env.insert(var.clone(), model.to_string());
            }
            ModelInjection::Arg { flag } => {
                if !self.args.iter().any(|arg| arg == flag) {
                    self.args.push(flag.clone());
                    self.args.push(model.to_string());
                }
            }
            ModelInjection::CodexConfig => {
                let current = self.env.get("CODEX_CONFIG").cloned();
                let merged = codex_config_with_model(current.as_deref(), model)?;
                self.env.insert("CODEX_CONFIG".to_string(), merged);
            }
            ModelInjection::None => {}
        }
        Ok(())
    }

    /// Whether this agent has a spawn-time channel to force a model.
    pub fn supports_model_injection(&self) -> bool {
        !matches!(self.model_injection, ModelInjection::None)
    }

    /// Context window size for this agent, if known.
    pub fn context_window_size(&self) -> Option<i64> {
        // Check registry first (may have custom override)
        let registry = get_registry();
        registry
            .iter()
            .find(|a| a.slug == self.provider_slug)
            .and_then(|a| a.context_window_size)
    }

    /// Transcript base directory for this agent, if known.
    pub fn transcript_base_dir(&self) -> Option<String> {
        let registry = get_registry();
        registry
            .iter()
            .find(|a| a.slug == self.provider_slug)
            .and_then(|a| a.transcript_base_dir.clone())
    }
}

/// Get list of all available ACP agents for the agent picker
pub fn list_available_agents() -> Vec<AcpAgentInfo> {
    #[cfg(debug_assertions)]
    let mock_binary =
        get_mock_agent_config("mock-model").map(|config| std::path::PathBuf::from(config.command));
    #[cfg(not(debug_assertions))]
    let mock_binary: Option<std::path::PathBuf> = None;
    list_available_agents_with_mock_binary(mock_binary.as_deref())
}

fn list_available_agents_with_mock_binary(
    mock_binary: Option<&std::path::Path>,
) -> Vec<AcpAgentInfo> {
    let mut agents = Vec::new();

    // Mock agents are available only when the debug fixture executable exists.
    #[cfg(debug_assertions)]
    if mock_binary.is_some_and(std::path::Path::is_file) {
        agents.push(AcpAgentInfo {
            agent: AgentKind::ClaudeCode, // Dummy, not really used
            agent_name: "mock-model".to_string(),
            display_name: "Mock ACP".to_string(),
            description: "Mock agent for testing".to_string(),
            provider_slug: "mock-acp".to_string(),
            is_installed: true,
            managed_by: None,
            unavailable: None,
        });
        agents.push(AcpAgentInfo {
            agent: AgentKind::ClaudeCode, // Dummy, not really used
            agent_name: "mock-model-alt".to_string(),
            display_name: "Mock ACP Alt".to_string(),
            description: "Alternate mock agent for testing".to_string(),
            provider_slug: "mock-acp-alt".to_string(),
            is_installed: true,
            managed_by: None,
            unavailable: None,
        });
    }
    #[cfg(not(debug_assertions))]
    let _ = mock_binary;

    // Include all agents from the registry (built-in + custom)
    for registered in get_registry() {
        if let Some(kind) = registered.kind {
            // Built-in agent: use existing detection logic
            agents.push(AcpAgentInfo::from_agent(kind));
        } else {
            // Custom agent: always shown, installation detection not applicable
            agents.push(AcpAgentInfo {
                agent: AgentKind::ClaudeCode, // Placeholder for custom agents
                agent_name: registered.slug.clone(),
                display_name: registered.name.clone(),
                description: registered.slug.clone(),
                provider_slug: registered.slug,
                is_installed: true,
                managed_by: None,
                unavailable: None,
            });
        }
    }

    agents
}

/// Remove stale `@zed-industries/claude-agent-acp*` and `codex-acp*` entries
/// from bun's install cache. These packages were renamed to the
/// `@agentclientprotocol` scope but leftover cache entries under the old scope
/// can interfere with resolution.
fn clean_stale_acp_cache(cache_dir: &std::path::Path) {
    let zed_scope = cache_dir.join("@zed-industries");
    let entries = match std::fs::read_dir(&zed_scope) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("claude-agent-acp") || name.starts_with("codex-acp") {
            let path = entry.path();
            tracing::info!("removing stale ACP cache entry: {}", path.display());
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(
                    "failed to remove stale ACP cache entry {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// Get ACP agent configuration for a given agent name
///
/// # Arguments
/// * `agent_name` - The agent identifier (e.g., "claude-code", "gemini")
///   Names are normalized to lowercase for case-insensitive matching.
///
/// # Returns
/// Configuration with provider_slug, command and args to spawn the agent subprocess
///
/// # Errors
/// Returns error if agent_name is not recognized
pub fn get_agent_config(agent_name: &str) -> Result<AcpAgentConfig> {
    // Normalize agent name: lowercase
    let normalized = agent_name.to_lowercase();

    // Try mock agents first (only available in debug builds)
    #[cfg(debug_assertions)]
    if let Some(config) = get_mock_agent_config(&normalized) {
        return Ok(config);
    }

    // Check the data-driven registry first for custom/overridden agents
    let registry = get_registry();
    if let Some(registered) = registry.iter().find(|a| a.slug == normalized)
        && let Some(ref dist) = registered.distribution
    {
        // Custom distribution: use the resolved distribution directly
        let (command, args, env) = match dist {
            ResolvedDistribution::Local { command, args, env } => {
                (command.clone(), args.clone(), env.clone())
            }
            ResolvedDistribution::Npx { package, args } => {
                let mut full_args = vec![package.clone()];
                full_args.extend(args.iter().cloned());
                ("npx".to_string(), full_args, HashMap::new())
            }
            ResolvedDistribution::Bunx { package, args } => {
                let mut full_args = vec![package.clone()];
                full_args.extend(args.iter().cloned());
                ("bunx".to_string(), full_args, HashMap::new())
            }
            ResolvedDistribution::Pipx { package, args } => {
                let mut full_args = vec!["run".to_string(), package.clone()];
                full_args.extend(args.iter().cloned());
                ("pipx".to_string(), full_args, HashMap::new())
            }
            ResolvedDistribution::Uvx { package, args } => {
                let mut full_args = vec![package.clone()];
                full_args.extend(args.iter().cloned());
                ("uvx".to_string(), full_args, HashMap::new())
            }
            // Nothing is started: the agent runs in this process. An account's environment is its
            // provider's home and, for an agent that drives a CLI, that CLI unwrapped, so
            // nothing in the process environment or a launcher script picks the account.
            ResolvedDistribution::Native { home, .. } => {
                let environment = match (
                    nori_config::parse_account_slug(&registered.slug),
                    home.as_deref(),
                ) {
                    (Some((provider, _)), Some(home)) => {
                        crate::accounts::native_environment(provider, home).map_err(|reason| {
                            anyhow::anyhow!("{} cannot start: {reason}", registered.name)
                        })?
                    }
                    // The provider's own login: where its launcher already keeps it.
                    _ => registered.kind.map_or_else(HashMap::new, |kind| {
                        crate::accounts::default_environment(crate::accounts::provider_of(kind))
                    }),
                };
                (String::new(), Vec::new(), environment)
            }
        };

        let install_hint = match dist {
            ResolvedDistribution::Local { command, .. } => {
                format!("ensure '{command}' is in your PATH")
            }
            ResolvedDistribution::Npx { package, .. } => {
                format!("npm install -g {package}")
            }
            ResolvedDistribution::Bunx { package, .. } => {
                format!("bun add -g {package}")
            }
            ResolvedDistribution::Pipx { package, .. } => {
                format!("pipx install {package}")
            }
            ResolvedDistribution::Uvx { package, .. } => {
                format!("uv tool install {package}")
            }
            ResolvedDistribution::Native { agent, .. } => {
                format!("this build of Nori has no built-in {agent} agent; rebuild with it enabled")
            }
        };
        let in_process = match dist {
            ResolvedDistribution::Native {
                agent,
                home,
                config,
            } => Some(crate::in_process::InProcessSpec {
                agent: agent.clone(),
                home: home.clone(),
                config: config.clone(),
                environment: HashMap::new(),
            }),
            _ => None,
        };

        return Ok(AcpAgentConfig {
            // An account runs its provider's kind of agent, so what depends on the kind (how it
            // signs in) follows the account.
            agent: registered
                .kind
                .or_else(|| crate::accounts::kind_of_slug(&registered.slug))
                .unwrap_or(AgentKind::ClaudeCode),
            provider_slug: registered.slug.clone(),
            command,
            args,
            env,
            provider_info: AcpProviderInfo {
                name: format!("{} ACP", registered.name),
                ..Default::default()
            },
            auth_hint: registered.auth_hint.clone().unwrap_or_default(),
            display_name: registered.name.clone(),
            install_hint,
            model_injection: registered.model_injection.clone(),
            advertise_subagents: true,
            in_process,
        });
    }

    // Try to parse as a built-in AgentKind (auto-detection path)
    if let Some(agent) = AgentKind::from_slug(&normalized) {
        let package_manager = detect_preferred_package_manager();

        if matches!(agent, AgentKind::ClaudeCode | AgentKind::Codex)
            && package_manager == PackageManager::Bun
        {
            let cache_dir = std::env::var("BUN_INSTALL_CACHE_DIR")
                .map(std::path::PathBuf::from)
                .or_else(|_| {
                    std::env::var("BUN_INSTALL")
                        .map(|p| std::path::PathBuf::from(p).join("install/cache"))
                })
                .unwrap_or_else(|_| {
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join(".bun/install/cache")
                });
            clean_stale_acp_cache(&cache_dir);
        }

        let (command, args) = match agent {
            // Claude and Codex use external ACP adapters
            AgentKind::ClaudeCode | AgentKind::Codex => (
                package_manager.command().to_string(),
                vec![agent.acp_package().to_string()],
            ),
            // Gemini starts its ACP agent with --acp.
            AgentKind::Gemini => (
                package_manager.command().to_string(),
                vec![agent.acp_package().to_string(), "--acp".to_string()],
            ),
            AgentKind::OpenCode => ("opencode".to_string(), vec!["acp".to_string()]),
            // The signed-in official CLI speaks ACP itself; its version decides whether it can.
            AgentKind::Antigravity => (
                crate::antigravity_cli::discover()?
                    .path
                    .to_string_lossy()
                    .into_owned(),
                crate::antigravity_cli::ACP_ARGS
                    .iter()
                    .map(|arg| (*arg).to_string())
                    .collect(),
            ),
        };

        // Workaround: the v0.30.0+ Claude ACP adapter resolves its native
        // binary via require.resolve() on platform-specific optional deps.
        // On glibc Linux, bunx installs both musl and glibc variants; the
        // adapter tries musl first, require.resolve succeeds (file exists),
        // but execution fails (no musl loader). Point CLAUDE_CODE_EXECUTABLE
        // at the system binary to bypass the broken resolution.
        let mut env = HashMap::new();
        if agent == AgentKind::ClaudeCode
            && let Ok(path) = which::which("claude")
        {
            env.insert(
                "CLAUDE_CODE_EXECUTABLE".to_string(),
                path.to_string_lossy().to_string(),
            );
        }
        if agent == AgentKind::Codex {
            let ambient_config = std::env::var("CODEX_CONFIG").ok();
            env.insert(
                "CODEX_CONFIG".to_string(),
                codex_config_for_nori(ambient_config.as_deref())?,
            );
        }

        return Ok(AcpAgentConfig {
            agent,
            provider_slug: agent.slug().to_string(),
            command,
            args,
            env,
            provider_info: AcpProviderInfo {
                name: format!("{} ACP", agent.display_name()),
                ..Default::default()
            },
            auth_hint: agent.auth_hint().to_string(),
            display_name: agent.display_name().to_string(),
            install_hint: match agent {
                AgentKind::Antigravity => format!(
                    "install the official Antigravity CLI {} or newer (`{}` upgrades it)",
                    crate::antigravity_cli::MINIMUM_VERSION,
                    crate::antigravity_cli::UPGRADE_COMMAND
                ),
                AgentKind::ClaudeCode
                | AgentKind::Codex
                | AgentKind::Gemini
                | AgentKind::OpenCode => format!("npm install -g {}", agent.npm_package()),
            },
            model_injection: agent.model_injection(),
            advertise_subagents: true,
            // Claude and Codex run inside Nori when this build has them, so there is no adapter
            // to start.
            in_process: native_name(agent)
                .filter(|name| crate::in_process::in_process_available(name))
                .map(|name| crate::in_process::InProcessSpec {
                    agent: name.to_string(),
                    home: None,
                    config: Vec::new(),
                    environment: HashMap::new(),
                }),
        });
    }

    anyhow::bail!("Unknown ACP agent: {agent_name}")
}

/// The built-in in-process agent that can stand in for `agent`'s adapter, if there is one.
fn native_name(agent: AgentKind) -> Option<&'static str> {
    match agent {
        AgentKind::ClaudeCode => Some("claude"),
        AgentKind::Codex => Some("codex"),
        AgentKind::Gemini => None,
        AgentKind::OpenCode => Some("opencode"),
        // The official CLI runs as its own process; the SDK adapter is the `antigravity-sdk`
        // native distribution.
        AgentKind::Antigravity => None,
    }
}

/// Get the display name for an agent by agent name.
///
/// Returns the human-readable display name if the agent is registered.
/// Falls back to the agent_name itself if not recognized.
pub fn get_agent_display_name(agent_name: &str) -> String {
    let normalized = agent_name.to_lowercase();

    // Mock agents (debug builds only)
    #[cfg(debug_assertions)]
    {
        if normalized == "mock-model" {
            return "Mock ACP".to_string();
        }
        if normalized == "mock-model-alt" {
            return "Mock ACP Alt".to_string();
        }
    }

    // Check the data-driven registry (includes both built-in and custom)
    let registry = get_registry();
    if let Some(registered) = registry.iter().find(|a| a.slug == normalized) {
        return registered.name.clone();
    }

    // Fallback: try AgentKind parsing for legacy aliases
    if let Some(agent) = AgentKind::from_slug(&normalized) {
        return agent.display_name().to_string();
    }

    // Fallback to agent name
    agent_name.to_string()
}

/// Get mock agent configuration (only available in debug builds)
#[cfg(debug_assertions)]
fn get_mock_agent_config(normalized: &str) -> Option<AcpAgentConfig> {
    match normalized {
        "mock-model" => {
            // Resolve path to mock_acp_agent binary.
            //
            // Priority:
            // 1. MOCK_ACP_AGENT_BIN environment variable (set by CI)
            // 2. Relative to current executable (for local development)
            let exe_path = if let Ok(env_path) = std::env::var("MOCK_ACP_AGENT_BIN") {
                tracing::debug!("Mock ACP agent path from MOCK_ACP_AGENT_BIN: {}", env_path);
                std::path::PathBuf::from(env_path)
            } else {
                // Fall back to resolving relative to current executable.
                // This handles both:
                // - Running as `codex` binary: target/{profile}/codex -> target/{profile}/
                // - Running as test binary: target/{profile}/deps/test -> target/{profile}/
                let mock_path = std::env::current_exe()
                    .ok()
                    .and_then(|p| {
                        p.parent().map(|parent| {
                            // Check if we're in a "deps" directory (test binary context)
                            let in_deps_dir = parent
                                .file_name()
                                .map(|name| name == "deps")
                                .unwrap_or(false);

                            if in_deps_dir {
                                parent.parent().map(|p| p.join("mock_acp_agent"))
                            } else {
                                Some(parent.join("mock_acp_agent"))
                            }
                        })
                    })
                    .flatten()
                    .unwrap_or_else(|| std::path::PathBuf::from("mock_acp_agent"));
                tracing::debug!("Mock ACP agent path resolved to: {}", mock_path.display());
                mock_path
            };

            Some(AcpAgentConfig {
                agent: AgentKind::ClaudeCode, // Dummy
                provider_slug: "mock-acp".to_string(),
                command: exe_path.to_string_lossy().to_string(),
                args: vec![],
                env: HashMap::from([(
                    "MOCK_AGENT_MODEL_NAME".to_string(),
                    "mock-model".to_string(),
                )]),
                provider_info: AcpProviderInfo {
                    name: "Mock ACP".to_string(),
                    ..Default::default()
                },
                auth_hint: "Mock agent - no authentication required.".to_string(),
                display_name: "Mock ACP".to_string(),
                install_hint: "Mock agent - no installation required".to_string(),
                model_injection: ModelInjection::Env {
                    var: "MOCK_AGENT_INJECTED_MODEL".to_string(),
                },
                advertise_subagents: true,
                in_process: None,
            })
        }
        "mock-model-alt" => {
            // Alternate mock model for E2E testing agent switching.
            // Uses the same binary as mock-model but with a different provider_slug
            // to verify that different agent configurations spawn separate subprocesses.
            let exe_path = if let Ok(env_path) = std::env::var("MOCK_ACP_AGENT_BIN") {
                tracing::debug!("Mock ACP agent path from MOCK_ACP_AGENT_BIN: {}", env_path);
                std::path::PathBuf::from(env_path)
            } else {
                let mock_path = std::env::current_exe()
                    .ok()
                    .and_then(|p| {
                        p.parent().map(|parent| {
                            let in_deps_dir = parent
                                .file_name()
                                .map(|name| name == "deps")
                                .unwrap_or(false);

                            if in_deps_dir {
                                parent.parent().map(|p| p.join("mock_acp_agent"))
                            } else {
                                Some(parent.join("mock_acp_agent"))
                            }
                        })
                    })
                    .flatten()
                    .unwrap_or_else(|| std::path::PathBuf::from("mock_acp_agent"));
                tracing::debug!("Mock ACP agent path resolved to: {}", mock_path.display());
                mock_path
            };

            Some(AcpAgentConfig {
                agent: AgentKind::ClaudeCode, // Dummy
                provider_slug: "mock-acp-alt".to_string(),
                command: exe_path.to_string_lossy().to_string(),
                args: vec![],
                env: HashMap::from([(
                    "MOCK_AGENT_MODEL_NAME".to_string(),
                    "mock-model-alt".to_string(),
                )]),
                provider_info: AcpProviderInfo {
                    name: "Mock ACP Alt".to_string(),
                    ..Default::default()
                },
                auth_hint: "Mock agent - no authentication required.".to_string(),
                display_name: "Mock ACP Alt".to_string(),
                install_hint: "Mock agent - no installation required".to_string(),
                model_injection: ModelInjection::Env {
                    var: "MOCK_AGENT_INJECTED_MODEL".to_string(),
                },
                advertise_subagents: true,
                in_process: None,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
