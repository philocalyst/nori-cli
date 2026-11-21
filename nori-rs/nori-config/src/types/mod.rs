//! Type definitions for Nori configuration

use crate::AskForApproval;
use crate::McpServerConfig;
use crate::SandboxMode;
use crate::SandboxPolicy;
use crate::ShellEnvironmentPolicy;
use crate::ShellEnvironmentPolicyToml;
use crate::TrustLevel;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// History persistence policy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HistoryPersistence {
    /// Save all history entries to disk.
    #[default]
    SaveAll,
    /// Do not write history to disk.
    None,
}

/// Which Chrome profile the `/browser` command launches against.
///
/// Secure-by-default: `Throwaway` shares no cookies, logins, or settings with
/// the user's real Chrome. The other tiers are explicit power-user opt-ins that
/// trade isolation for persistence or real credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserProfileMode {
    /// Fresh throwaway profile per launch, wiped on shutdown. No shared state.
    #[default]
    Throwaway,
    /// Persistent nori-owned profile (`<nori_home>/browser-profile`) reused
    /// across launches. Logins survive, but it stays isolated from real Chrome.
    Persistent,
    /// The user's real default Chrome profile, with all their logins/cookies.
    System,
}

impl BrowserProfileMode {
    /// Human-readable name for display in the TUI picker.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Throwaway => "Throwaway",
            Self::Persistent => "Persistent nori profile",
            Self::System => "Real Chrome profile",
        }
    }

    /// One-line description shown beneath each picker row.
    pub fn description(&self) -> &'static str {
        match self {
            Self::Throwaway => "Fresh profile, no logins or cookies (secure default)",
            Self::Persistent => {
                "Reuses a nori-owned profile; logins persist, isolated from your Chrome"
            }
            Self::System => "Your real Chrome logins & cookies — only use on trusted pages",
        }
    }

    /// TOML string representation for persistence.
    pub fn toml_value(&self) -> &'static str {
        match self {
            Self::Throwaway => "throwaway",
            Self::Persistent => "persistent",
            Self::System => "system",
        }
    }

    /// All variants in order, for building picker UIs.
    pub fn all_variants() -> &'static [BrowserProfileMode] {
        &[Self::Throwaway, Self::Persistent, Self::System]
    }
}

/// Default agent for ACP-only mode
pub const DEFAULT_AGENT: &str = "claude-code";

// ============================================================================
// Agent Configuration (TOML schema)
// ============================================================================

/// A single agent definition from `[[agents]]` in config.toml.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentConfigToml {
    /// Display name shown in the agent picker (e.g. "Claude Code")
    pub name: String,
    /// Machine identifier used as a cmdline arg or in UIs (e.g. "claude-code")
    pub slug: String,
    /// How to invoke this agent
    pub distribution: AgentDistributionToml,
    /// Optional context window size override (in tokens)
    pub context_window_size: Option<i64>,
    /// Optional auth instructions (displayed on auth failures)
    pub auth_hint: Option<String>,
    /// Optional transcript base directory (relative to home)
    pub transcript_base_dir: Option<String>,
    /// Optional declaration of how to force a specific model on this agent at
    /// spawn time. Custom ACP clients advertise only a subset of the models
    /// they can actually run; this tells nori which out-of-band channel carries
    /// the model id so a user-chosen model that the picker rejects can still be
    /// applied by restarting the session. Built-in agents (Claude, Codex,
    /// Gemini) know their own channel and ignore this field.
    #[serde(default)]
    pub model_override: Option<ModelOverrideToml>,
}

/// How a custom agent accepts a model id at spawn time. Set exactly one of
/// `env` or `arg`; `env` wins if both are set.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ModelOverrideToml {
    /// Environment variable name to set to the model id (e.g. "ANTHROPIC_MODEL").
    pub env: Option<String>,
    /// CLI flag to append followed by the model id (e.g. "--model").
    pub arg: Option<String>,
}

/// Distribution configuration for an agent.
///
/// Exactly one variant must be set. The field names correspond to the
/// package manager or distribution method.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AgentDistributionToml {
    /// Local binary execution
    pub local: Option<LocalDistribution>,
    /// Node.js: `npx <package> [args...]`
    pub npx: Option<PackageDistribution>,
    /// Bun: `bunx <package> [args...]`
    pub bunx: Option<PackageDistribution>,
    /// Python: `pipx run <package> [args...]`
    pub pipx: Option<PackageDistribution>,
    /// Python (uv): `uvx <package> [args...]`
    pub uvx: Option<PackageDistribution>,
    /// An agent built into Nori and run inside its own process, with no subprocess at all.
    pub native: Option<NativeDistribution>,
    // Future: cargo (cargo-binstall / cargo install)
    // Future: binary (platform-specific archive downloads)
}

impl AgentDistributionToml {
    /// Validate that exactly one distribution variant is set.
    fn validate(&self) -> Result<(), String> {
        let count = [
            self.local.is_some(),
            self.npx.is_some(),
            self.bunx.is_some(),
            self.pipx.is_some(),
            self.uvx.is_some(),
            self.native.is_some(),
        ]
        .iter()
        .filter(|&&b| b)
        .count();

        if count == 0 {
            return Err(
                "Agent distribution must specify exactly one of: local, npx, bunx, pipx, uvx, native"
                    .to_string(),
            );
        }
        if count > 1 {
            return Err(
                "Agent distribution must specify exactly one of: local, npx, bunx, pipx, uvx, native (found multiple)"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// Validate and resolve into a clean enum variant.
    pub fn resolve(&self) -> Result<ResolvedDistribution, String> {
        self.validate()?;

        if let Some(local) = &self.local {
            return Ok(ResolvedDistribution::Local {
                command: local.command.clone(),
                args: local.args.clone(),
                env: local.env.clone(),
            });
        }
        if let Some(npx) = &self.npx {
            return Ok(ResolvedDistribution::Npx {
                package: npx.package.clone(),
                args: npx.args.clone(),
            });
        }
        if let Some(bunx) = &self.bunx {
            return Ok(ResolvedDistribution::Bunx {
                package: bunx.package.clone(),
                args: bunx.args.clone(),
            });
        }
        if let Some(pipx) = &self.pipx {
            return Ok(ResolvedDistribution::Pipx {
                package: pipx.package.clone(),
                args: pipx.args.clone(),
            });
        }
        if let Some(uvx) = &self.uvx {
            return Ok(ResolvedDistribution::Uvx {
                package: uvx.package.clone(),
                args: uvx.args.clone(),
            });
        }
        if let Some(native) = &self.native {
            return Ok(ResolvedDistribution::Native {
                agent: native.agent.clone(),
                home: native.home.as_ref().map(std::path::PathBuf::from),
                config: native.config.clone(),
            });
        }
        unreachable!("validate() ensures exactly one variant is set")
    }
}

/// Local binary distribution: direct command execution.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct LocalDistribution {
    /// Path to the executable
    pub command: String,
    /// Arguments to pass to the command
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment variables to set
    #[serde(default)]
    pub env: HashMap<String, String>,
}

/// An agent compiled into Nori, served in-process over an in-memory pipe.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NativeDistribution {
    /// Which built-in agent: `claude` or `codex`.
    pub agent: String,
    /// Where this account's state and login live (Codex's `CODEX_HOME`). Set per agent, so two
    /// accounts can run side by side in one process.
    #[serde(default)]
    pub home: Option<String>,
    /// `key=value` overrides as `codex -c` takes them, e.g. `model="o3"`.
    #[serde(default)]
    pub config: Vec<String>,
}

/// Package manager distribution: `<manager> <package> [args...]`
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct PackageDistribution {
    /// Package name (e.g. "@google/gemini-cli", "kimi-cli")
    pub package: String,
    /// Extra arguments to pass after the package name
    #[serde(default)]
    pub args: Vec<String>,
}

/// Resolved (validated) distribution — exactly one variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedDistribution {
    /// Local binary: direct command execution
    Local {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Node.js: `npx <package> [args...]`
    Npx { package: String, args: Vec<String> },
    /// Bun: `bunx <package> [args...]`
    Bunx { package: String, args: Vec<String> },
    /// Python: `pipx run <package> [args...]`
    Pipx { package: String, args: Vec<String> },
    /// Python (uv): `uvx <package> [args...]`
    Uvx { package: String, args: Vec<String> },
    /// A built-in agent run inside Nori's own process.
    Native {
        agent: String,
        home: Option<std::path::PathBuf>,
        config: Vec<String>,
    },
    // Future: Cargo { crate_name: String, version: Option<String>, binary: Option<String> }
    // Future: Binary { url: String, platforms: HashMap<String, BinaryPlatformConfig> }
}

/// TOML-deserializable config structure (all fields optional)
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct NoriConfigToml {
    /// The ACP agent to use (e.g., "claude-code", "codex", "gemini")
    /// This is persisted to track the user's agent preference.
    pub agent: Option<String>,

    /// Sandbox mode for command execution
    pub sandbox_mode: Option<SandboxMode>,

    /// Settings applied when the sandbox uses workspace-write mode.
    pub sandbox_workspace_write: Option<SandboxWorkspaceWrite>,

    /// Approval policy for commands
    pub approval_policy: Option<AskForApproval>,

    /// Environment inherited by sandboxed commands.
    #[serde(default)]
    pub shell_environment_policy: ShellEnvironmentPolicyToml,

    /// History persistence policy
    pub history_persistence: Option<HistoryPersistence>,

    /// Which Chrome profile the `/browser` command launches against.
    pub browser_profile: Option<BrowserProfileMode>,

    /// External notifier command and arguments.
    pub notify: Option<Vec<String>>,

    /// Tell agents that nori shows subagent sessions (`clientCapabilities.subagents`).
    /// Turn this off for an adapter that misbehaves when it is advertised. Default: true.
    pub subagent_sessions: Option<bool>,

    /// Give every session the nudox code-intelligence MCP server when `nudox` is installed.
    /// Set `nudox = false` to turn it off. Default: true.
    pub nudox: Option<bool>,

    /// ACP wire proxy logging settings
    #[serde(default)]
    pub acp_proxy: AcpProxyConfigToml,

    /// TUI settings
    #[serde(default)]
    pub tui: TuiConfigToml,

    /// MCP server configurations (optional)
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Session lifecycle hooks
    #[serde(default)]
    pub hooks: HooksConfigToml,

    /// How changes are committed and proposed
    #[serde(default)]
    pub git: GitConfigToml,

    /// Default model overrides per agent (e.g., claude-code = "haiku")
    #[serde(default)]
    pub default_models: HashMap<String, String>,

    /// Custom agent definitions
    #[serde(default)]
    pub agents: Vec<AgentConfigToml>,

    /// Accounts of built-in providers, each run in Nori's own process (see `accounts`).
    #[serde(default)]
    pub accounts: Vec<crate::AccountToml>,

    /// Cloud session settings
    #[serde(default)]
    pub cloud: CloudConfigToml,

    /// Whether to check for Nori updates at startup.
    pub check_for_update_on_startup: Option<bool>,

    /// Disable burst-paste detection in the prompt composer.
    pub disable_paste_burst: Option<bool>,

    /// Nori-owned feature switches.
    #[serde(default)]
    pub features: FeaturesToml,

    /// User acknowledgement state for safety notices.
    #[serde(default)]
    pub notice: Notice,

    /// Per-project trust settings, keyed by project path.
    #[serde(default)]
    pub projects: HashMap<String, ProjectConfig>,
}

/// Workspace-write sandbox settings from `config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SandboxWorkspaceWrite {
    #[serde(default)]
    pub writable_roots: Vec<PathBuf>,
    #[serde(default)]
    pub network_access: bool,
    #[serde(default)]
    pub exclude_tmpdir_env_var: bool,
    #[serde(default)]
    pub exclude_slash_tmp: bool,
}

/// Feature switches that still affect the Nori runtime.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FeaturesToml {
    pub enable_experimental_windows_sandbox: Option<bool>,
}

/// Persisted safety notice acknowledgements.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Notice {
    pub hide_full_access_warning: Option<bool>,
    pub hide_world_writable_warning: Option<bool>,
}

/// TOML settings for cloud session integration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct CloudConfigToml {
    /// Broker URL for cloud sessions (e.g., "https://nori-broker.myorg.fly.dev")
    pub broker_url: Option<String>,
}

/// TOML settings for ACP wire proxy logging.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct AcpProxyConfigToml {
    /// Whether to record raw ACP JSON-RPC messages to disk.
    pub enabled: Option<bool>,
}

/// Resolved ACP wire proxy logging settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcpProxyConfig {
    /// Whether wire logging is enabled.
    pub enabled: bool,
    /// Directory where per-child JSONL wire logs are written.
    pub log_dir: PathBuf,
}

impl AcpProxyConfig {
    /// Build resolved proxy settings from TOML and the Nori home directory.
    pub fn from_toml(toml: AcpProxyConfigToml, nori_home: &std::path::Path) -> Self {
        Self {
            enabled: toml.enabled.unwrap_or(false),
            log_dir: nori_home.join("acp-wire"),
        }
    }

    /// Disabled proxy settings for tests and direct internal callers.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            log_dir: PathBuf::new(),
        }
    }
}

/// Whether terminal notifications (OSC 9) are enabled or disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalNotifications {
    Enabled,
    Disabled,
}

/// Whether OS-level desktop notifications are enabled or disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsNotifications {
    Enabled,
    Disabled,
}

/// How long after idle before sending a notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum NotifyAfterIdle {
    #[default]
    #[serde(rename = "5s")]
    FiveSeconds,
    #[serde(rename = "10s")]
    TenSeconds,
    #[serde(rename = "30s")]
    ThirtySeconds,
    #[serde(rename = "60s")]
    SixtySeconds,
    #[serde(rename = "disabled")]
    Disabled,
}

impl NotifyAfterIdle {
    /// Returns the duration for the idle timeout, or `None` if disabled.
    pub fn as_duration(&self) -> Option<Duration> {
        match self {
            Self::FiveSeconds => Some(Duration::from_secs(5)),
            Self::TenSeconds => Some(Duration::from_secs(10)),
            Self::ThirtySeconds => Some(Duration::from_secs(30)),
            Self::SixtySeconds => Some(Duration::from_secs(60)),
            Self::Disabled => None,
        }
    }

    /// Human-readable name for display in the TUI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::FiveSeconds => "5 seconds",
            Self::TenSeconds => "10 seconds",
            Self::ThirtySeconds => "30 seconds",
            Self::SixtySeconds => "1 minute",
            Self::Disabled => "Disabled",
        }
    }

    /// TOML string representation for persistence.
    pub fn toml_value(&self) -> &'static str {
        match self {
            Self::FiveSeconds => "5s",
            Self::TenSeconds => "10s",
            Self::ThirtySeconds => "30s",
            Self::SixtySeconds => "60s",
            Self::Disabled => "disabled",
        }
    }

    /// All variants in order, for building picker UIs.
    pub fn all_variants() -> &'static [NotifyAfterIdle] {
        &[
            Self::FiveSeconds,
            Self::TenSeconds,
            Self::ThirtySeconds,
            Self::SixtySeconds,
            Self::Disabled,
        ]
    }
}

// ============================================================================
// Auto Worktree Configuration
// ============================================================================

/// Whether to automatically create a git worktree at session start.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutoWorktree {
    /// Always create a worktree automatically.
    Automatic,
    /// Ask the user at session start whether to create a worktree.
    Ask,
    /// Never create a worktree automatically.
    #[default]
    Off,
}

impl AutoWorktree {
    /// Human-readable name for display in the TUI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Automatic => "Automatic",
            Self::Ask => "Ask",
            Self::Off => "Off",
        }
    }

    /// TOML string representation for persistence.
    pub fn toml_value(&self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::Ask => "ask",
            Self::Off => "off",
        }
    }

    /// All variants in order, for building picker UIs.
    pub fn all_variants() -> &'static [AutoWorktree] {
        &[Self::Automatic, Self::Ask, Self::Off]
    }

    /// Returns true if a worktree should be created (either automatically or
    /// after asking and getting confirmation).
    pub fn is_enabled(&self) -> bool {
        matches!(self, Self::Automatic | Self::Ask)
    }
}

impl Serialize for AutoWorktree {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.toml_value())
    }
}

impl<'de> Deserialize<'de> for AutoWorktree {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct AutoWorktreeVisitor;

        impl<'de> serde::de::Visitor<'de> for AutoWorktreeVisitor {
            type Value = AutoWorktree;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a boolean or one of \"automatic\", \"ask\", \"off\"")
            }

            fn visit_bool<E>(self, value: bool) -> Result<AutoWorktree, E>
            where
                E: serde::de::Error,
            {
                Ok(if value {
                    AutoWorktree::Automatic
                } else {
                    AutoWorktree::Off
                })
            }

            fn visit_str<E>(self, value: &str) -> Result<AutoWorktree, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "automatic" => Ok(AutoWorktree::Automatic),
                    "ask" => Ok(AutoWorktree::Ask),
                    "off" => Ok(AutoWorktree::Off),
                    _ => Err(E::unknown_variant(value, &["automatic", "ask", "off"])),
                }
            }
        }

        deserializer.deserialize_any(AutoWorktreeVisitor)
    }
}

// ============================================================================
// Vim Enter Behavior Configuration
// ============================================================================

/// How the Enter key behaves when vim mode is active.
///
/// This setting doubles as the vim mode on/off switch: `Off` disables vim mode
/// entirely, while the other variants enable vim mode with the chosen Enter
/// key semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VimEnterBehavior {
    /// Enter inserts a newline in INSERT mode, submits in NORMAL mode.
    Newline,
    /// Enter submits in INSERT mode, inserts a newline in NORMAL mode.
    Submit,
    /// Enter submits in both INSERT and NORMAL mode.
    AlwaysSubmit,
    /// Vim mode is disabled.
    #[default]
    Off,
}

impl VimEnterBehavior {
    /// Human-readable name for display in the TUI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::Newline => "Submit in NORMAL",
            Self::Submit => "Submit in INSERT",
            Self::AlwaysSubmit => "Always Submit",
            Self::Off => "Off",
        }
    }

    /// TOML string representation for persistence.
    pub fn toml_value(&self) -> &'static str {
        match self {
            Self::Newline => "newline",
            Self::Submit => "submit",
            Self::AlwaysSubmit => "always_submit",
            Self::Off => "off",
        }
    }

    /// All variants in order, for building picker UIs.
    pub fn all_variants() -> &'static [VimEnterBehavior] {
        &[Self::Newline, Self::Submit, Self::AlwaysSubmit, Self::Off]
    }

    /// Returns true if vim mode is enabled (i.e. not Off).
    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Off)
    }
}

impl Serialize for VimEnterBehavior {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.toml_value())
    }
}

impl<'de> Deserialize<'de> for VimEnterBehavior {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct VimEnterBehaviorVisitor;

        impl<'de> serde::de::Visitor<'de> for VimEnterBehaviorVisitor {
            type Value = VimEnterBehavior;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str(
                    "a boolean or one of \"newline\", \"submit\", \"always_submit\", \"off\"",
                )
            }

            fn visit_bool<E>(self, value: bool) -> Result<VimEnterBehavior, E>
            where
                E: serde::de::Error,
            {
                Ok(if value {
                    VimEnterBehavior::Submit
                } else {
                    VimEnterBehavior::Off
                })
            }

            fn visit_str<E>(self, value: &str) -> Result<VimEnterBehavior, E>
            where
                E: serde::de::Error,
            {
                match value {
                    "newline" => Ok(VimEnterBehavior::Newline),
                    "submit" => Ok(VimEnterBehavior::Submit),
                    "always_submit" => Ok(VimEnterBehavior::AlwaysSubmit),
                    "off" => Ok(VimEnterBehavior::Off),
                    _ => Err(E::unknown_variant(
                        value,
                        &["newline", "submit", "always_submit", "off"],
                    )),
                }
            }
        }

        deserializer.deserialize_any(VimEnterBehaviorVisitor)
    }
}

// ============================================================================
// Script Timeout Configuration
// ============================================================================

/// A freeform duration string for script execution timeouts (e.g. "30s", "2m").
///
/// Supported suffixes: `s` (seconds), `m` (minutes). The raw string is
/// preserved for display and TOML round-tripping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptTimeout(String, Duration);

impl ScriptTimeout {
    /// Default timeout: 30 seconds.
    const DEFAULT_SECS: u64 = 30;

    /// Parse a duration string like "30s" or "2m".
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        let duration = Self::parse_duration(s).unwrap_or(Duration::from_secs(Self::DEFAULT_SECS));
        Self(s.to_string(), duration)
    }

    fn parse_duration(s: &str) -> Option<Duration> {
        let s = s.trim();
        if let Some(num) = s.strip_suffix('s') {
            num.parse::<u64>().ok().map(Duration::from_secs)
        } else if let Some(num) = s.strip_suffix('m') {
            num.parse::<u64>().ok().map(|m| Duration::from_secs(m * 60))
        } else {
            s.parse::<u64>().ok().map(Duration::from_secs)
        }
    }

    /// The resolved duration.
    pub fn as_duration(&self) -> Duration {
        self.1
    }

    /// Human-readable name for display in the TUI.
    pub fn display_name(&self) -> &str {
        &self.0
    }

    /// TOML string representation for persistence.
    pub fn toml_value(&self) -> &str {
        &self.0
    }

    /// Common timeout values for building picker UIs.
    pub fn all_common_values() -> Vec<ScriptTimeout> {
        vec![
            ScriptTimeout::from_str("10s"),
            ScriptTimeout::from_str("30s"),
            ScriptTimeout::from_str("1m"),
            ScriptTimeout::from_str("2m"),
            ScriptTimeout::from_str("5m"),
        ]
    }
}

impl Default for ScriptTimeout {
    fn default() -> Self {
        Self("30s".to_string(), Duration::from_secs(Self::DEFAULT_SECS))
    }
}

impl Serialize for ScriptTimeout {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ScriptTimeout {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(ScriptTimeout::from_str(&s))
    }
}

// ============================================================================
// Hotkey Configuration
// ============================================================================

/// A configurable hotkey action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeyAction {
    /// Open the transcript pager overlay.
    OpenTranscript,
    /// Open an external editor for composing.
    OpenEditor,
    /// Move cursor one character backward.
    MoveBackwardChar,
    /// Move cursor one character forward.
    MoveForwardChar,
    /// Move cursor to beginning of line.
    MoveBeginningOfLine,
    /// Move cursor to end of line.
    MoveEndOfLine,
    /// Move cursor one word backward.
    MoveBackwardWord,
    /// Move cursor one word forward.
    MoveForwardWord,
    /// Delete one character backward.
    DeleteBackwardChar,
    /// Delete one character forward.
    DeleteForwardChar,
    /// Delete one word backward.
    DeleteBackwardWord,
    /// Kill text to end of line.
    KillToEndOfLine,
    /// Kill text to beginning of line.
    KillToBeginningOfLine,
    /// Yank (paste) killed text.
    Yank,
    /// Search prompt history (reverse search).
    HistorySearch,
    /// Toggle the pinned plan drawer between collapsed and expanded.
    TogglePlanDrawer,
    /// Hide everything above the prompt, leaving scrollback alone.
    Redraw,
    /// Switch to the next model the agent offers.
    NextModel,
    /// Switch to the previous model the agent offers.
    PreviousModel,
    /// Step to the next effort level (reasoning effort) the agent offers.
    NextEffort,
    /// Step to the previous effort level the agent offers.
    PreviousEffort,
    /// Have an agent land the working tree's changes as small, focused commits.
    CommitChanges,
    /// Open the subagent switcher.
    SwitchSubagent,
    /// Step to the next model mode (standard, fast, flex, priority) the agent offers.
    NextModelMode,
    /// Step to the previous model mode the agent offers.
    PreviousModelMode,
}

impl HotkeyAction {
    /// Human-readable name for display in the TUI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::OpenTranscript => "Open Transcript",
            Self::OpenEditor => "Open Editor",
            Self::MoveBackwardChar => "Move Backward Char",
            Self::MoveForwardChar => "Move Forward Char",
            Self::MoveBeginningOfLine => "Move to Line Start",
            Self::MoveEndOfLine => "Move to Line End",
            Self::MoveBackwardWord => "Move Backward Word",
            Self::MoveForwardWord => "Move Forward Word",
            Self::DeleteBackwardChar => "Delete Backward Char",
            Self::DeleteForwardChar => "Delete Forward Char",
            Self::DeleteBackwardWord => "Delete Backward Word",
            Self::KillToEndOfLine => "Kill to Line End",
            Self::KillToBeginningOfLine => "Kill to Line Start",
            Self::Yank => "Yank",
            Self::HistorySearch => "History Search",
            Self::TogglePlanDrawer => "Toggle Plan Drawer",
            Self::Redraw => "Clear Screen",
            Self::NextModel => "Next Model",
            Self::PreviousModel => "Previous Model",
            Self::NextEffort => "Next Effort Level",
            Self::PreviousEffort => "Previous Effort Level",
            Self::CommitChanges => "Commit Changes",
            Self::SwitchSubagent => "Switch Subagent",
            Self::NextModelMode => "Next Model Mode",
            Self::PreviousModelMode => "Previous Model Mode",
        }
    }

    /// Description for the hotkey picker.
    pub fn description(&self) -> &'static str {
        match self {
            Self::OpenTranscript => "Browse conversation history on the primary screen",
            Self::OpenEditor => "Open an external editor to compose a message",
            Self::MoveBackwardChar => "Move cursor one character backward",
            Self::MoveForwardChar => "Move cursor one character forward",
            Self::MoveBeginningOfLine => "Move cursor to beginning of line",
            Self::MoveEndOfLine => "Move cursor to end of line",
            Self::MoveBackwardWord => "Move cursor one word backward",
            Self::MoveForwardWord => "Move cursor one word forward",
            Self::DeleteBackwardChar => "Delete one character backward",
            Self::DeleteForwardChar => "Delete one character forward",
            Self::DeleteBackwardWord => "Delete one word backward",
            Self::KillToEndOfLine => "Kill text to end of line",
            Self::KillToBeginningOfLine => "Kill text to beginning of line",
            Self::Yank => "Yank (paste) killed text",
            Self::HistorySearch => "Search prompt history (reverse search)",
            Self::TogglePlanDrawer => {
                "Toggle the pinned plan drawer between collapsed and expanded"
            }
            Self::Redraw => "Hide everything above the prompt without touching scrollback",
            Self::NextModel => "Switch to the next model the agent offers",
            Self::PreviousModel => "Switch to the previous model the agent offers",
            Self::NextEffort => "Step to the next effort level the agent offers",
            Self::PreviousEffort => "Step to the previous effort level the agent offers",
            Self::CommitChanges => {
                "Have an agent commit the working tree's changes as small, focused commits"
            }
            Self::SwitchSubagent => {
                "Open the subagent switcher: find, preview and switch to a subagent"
            }
            Self::NextModelMode => {
                "Step to the next model mode the agent offers (flex, standard, fast, priority)"
            }
            Self::PreviousModelMode => "Step to the previous model mode the agent offers",
        }
    }

    /// The TOML key name for this action under `[tui.hotkeys]`.
    pub fn toml_key(&self) -> &'static str {
        match self {
            Self::OpenTranscript => "open_transcript",
            Self::OpenEditor => "open_editor",
            Self::MoveBackwardChar => "move_backward_char",
            Self::MoveForwardChar => "move_forward_char",
            Self::MoveBeginningOfLine => "move_beginning_of_line",
            Self::MoveEndOfLine => "move_end_of_line",
            Self::MoveBackwardWord => "move_backward_word",
            Self::MoveForwardWord => "move_forward_word",
            Self::DeleteBackwardChar => "delete_backward_char",
            Self::DeleteForwardChar => "delete_forward_char",
            Self::DeleteBackwardWord => "delete_backward_word",
            Self::KillToEndOfLine => "kill_to_end_of_line",
            Self::KillToBeginningOfLine => "kill_to_beginning_of_line",
            Self::Yank => "yank",
            Self::HistorySearch => "history_search",
            Self::TogglePlanDrawer => "toggle_plan_drawer",
            Self::Redraw => "redraw",
            Self::NextModel => "next_model",
            Self::PreviousModel => "previous_model",
            Self::NextEffort => "next_effort",
            Self::PreviousEffort => "previous_effort",
            Self::CommitChanges => "commit_changes",
            Self::SwitchSubagent => "switch_subagent",
            Self::NextModelMode => "next_model_mode",
            Self::PreviousModelMode => "previous_model_mode",
        }
    }

    /// The default binding string for this action.
    pub fn default_binding(&self) -> &'static str {
        match self {
            Self::OpenTranscript => "ctrl+t",
            Self::OpenEditor => "ctrl+g",
            Self::MoveBackwardChar => "ctrl+b",
            Self::MoveForwardChar => "ctrl+f",
            Self::MoveBeginningOfLine => "ctrl+a",
            Self::MoveEndOfLine => "ctrl+e",
            Self::MoveBackwardWord => "alt+b",
            Self::MoveForwardWord => "alt+f",
            Self::DeleteBackwardChar => "ctrl+h",
            Self::DeleteForwardChar => "ctrl+d",
            Self::DeleteBackwardWord => "ctrl+w",
            Self::KillToEndOfLine => "alt+k",
            Self::KillToBeginningOfLine => "ctrl+u",
            Self::Yank => "ctrl+y",
            Self::HistorySearch => "ctrl+r",
            Self::TogglePlanDrawer => "ctrl+o",
            Self::Redraw => "ctrl+l",
            Self::NextModel => "ctrl+n",
            Self::PreviousModel => "ctrl+p",
            Self::NextEffort => "ctrl+j",
            Self::PreviousEffort => "ctrl+k",
            Self::CommitChanges => "alt+g",
            Self::SwitchSubagent => "ctrl+space",
            Self::NextModelMode => "alt+.",
            Self::PreviousModelMode => "alt+,",
        }
    }

    /// All hotkey actions, in display order.
    pub fn all_actions() -> &'static [HotkeyAction] {
        &[
            Self::OpenTranscript,
            Self::OpenEditor,
            Self::MoveBackwardChar,
            Self::MoveForwardChar,
            Self::MoveBeginningOfLine,
            Self::MoveEndOfLine,
            Self::MoveBackwardWord,
            Self::MoveForwardWord,
            Self::DeleteBackwardChar,
            Self::DeleteForwardChar,
            Self::DeleteBackwardWord,
            Self::KillToEndOfLine,
            Self::KillToBeginningOfLine,
            Self::Yank,
            Self::HistorySearch,
            Self::TogglePlanDrawer,
            Self::Redraw,
            Self::NextModel,
            Self::PreviousModel,
            Self::NextEffort,
            Self::PreviousEffort,
            Self::CommitChanges,
            Self::SwitchSubagent,
            Self::NextModelMode,
            Self::PreviousModelMode,
        ]
    }
}

impl fmt::Display for HotkeyAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// A hotkey binding represented as a string (e.g. "ctrl+t", "alt+g", "none").
///
/// The string format is: `[modifier+]key` where modifier is `ctrl`, `alt`, or `shift`,
/// and key is a single character, `enter`, `esc`, `f1`-`f12`, etc.
/// The special value `"none"` means the action is unbound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyBinding(Option<String>);

impl HotkeyBinding {
    /// Create a binding from a key string like "ctrl+t".
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        if s == "none" {
            Self(None)
        } else {
            Self(Some(s.to_lowercase()))
        }
    }

    /// Create an unbound (none) binding.
    pub fn none() -> Self {
        Self(None)
    }

    /// Returns true if this binding is unbound.
    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }

    /// Returns the binding string, or "none" if unbound.
    pub fn as_str(&self) -> &str {
        match &self.0 {
            Some(s) => s,
            None => "none",
        }
    }

    /// Human-readable display string (e.g. "ctrl + t" or "unbound").
    pub fn display_name(&self) -> String {
        match &self.0 {
            Some(s) => s.replace('+', " + "),
            None => "unbound".to_string(),
        }
    }

    /// TOML string for persistence.
    pub fn toml_value(&self) -> String {
        match &self.0 {
            Some(s) => s.clone(),
            None => "none".to_string(),
        }
    }
}

impl Serialize for HotkeyBinding {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.toml_value().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HotkeyBinding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(HotkeyBinding::from_str(&s))
    }
}

/// TOML-deserializable hotkey configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HotkeyConfigToml {
    /// Hotkey for opening the transcript pager.
    pub open_transcript: Option<HotkeyBinding>,
    /// Hotkey for opening an external editor.
    pub open_editor: Option<HotkeyBinding>,
    /// Hotkey for moving cursor one character backward.
    pub move_backward_char: Option<HotkeyBinding>,
    /// Hotkey for moving cursor one character forward.
    pub move_forward_char: Option<HotkeyBinding>,
    /// Hotkey for moving cursor to beginning of line.
    pub move_beginning_of_line: Option<HotkeyBinding>,
    /// Hotkey for moving cursor to end of line.
    pub move_end_of_line: Option<HotkeyBinding>,
    /// Hotkey for moving cursor one word backward.
    pub move_backward_word: Option<HotkeyBinding>,
    /// Hotkey for moving cursor one word forward.
    pub move_forward_word: Option<HotkeyBinding>,
    /// Hotkey for deleting one character backward.
    pub delete_backward_char: Option<HotkeyBinding>,
    /// Hotkey for deleting one character forward.
    pub delete_forward_char: Option<HotkeyBinding>,
    /// Hotkey for deleting one word backward.
    pub delete_backward_word: Option<HotkeyBinding>,
    /// Hotkey for killing text to end of line.
    pub kill_to_end_of_line: Option<HotkeyBinding>,
    /// Hotkey for killing text to beginning of line.
    pub kill_to_beginning_of_line: Option<HotkeyBinding>,
    /// Hotkey for yanking (pasting) killed text.
    pub yank: Option<HotkeyBinding>,
    /// Hotkey for searching prompt history (reverse search).
    pub history_search: Option<HotkeyBinding>,
    /// Hotkey for toggling the pinned plan drawer.
    pub toggle_plan_drawer: Option<HotkeyBinding>,
    /// Hotkey for repainting the screen.
    pub redraw: Option<HotkeyBinding>,
    /// Hotkey for switching to the next model the agent offers.
    pub next_model: Option<HotkeyBinding>,
    /// Hotkey for switching to the previous model the agent offers.
    pub previous_model: Option<HotkeyBinding>,
    /// Hotkey for stepping to the next effort level the agent offers.
    pub next_effort: Option<HotkeyBinding>,
    /// Hotkey for stepping to the previous effort level the agent offers.
    pub previous_effort: Option<HotkeyBinding>,
    /// Hotkey for having an agent commit the working tree's changes.
    pub commit_changes: Option<HotkeyBinding>,
    /// Hotkey for opening the subagent switcher.
    pub switch_subagent: Option<HotkeyBinding>,
    /// Hotkey for stepping to the next model mode the agent offers.
    pub next_model_mode: Option<HotkeyBinding>,
    /// Hotkey for stepping to the previous model mode the agent offers.
    pub previous_model_mode: Option<HotkeyBinding>,
}

/// Resolved hotkey configuration with defaults applied.
#[derive(Debug, Clone)]
pub struct HotkeyConfig {
    /// Hotkey for opening the transcript pager.
    pub open_transcript: HotkeyBinding,
    /// Hotkey for opening an external editor.
    pub open_editor: HotkeyBinding,
    /// Hotkey for moving cursor one character backward.
    pub move_backward_char: HotkeyBinding,
    /// Hotkey for moving cursor one character forward.
    pub move_forward_char: HotkeyBinding,
    /// Hotkey for moving cursor to beginning of line.
    pub move_beginning_of_line: HotkeyBinding,
    /// Hotkey for moving cursor to end of line.
    pub move_end_of_line: HotkeyBinding,
    /// Hotkey for moving cursor one word backward.
    pub move_backward_word: HotkeyBinding,
    /// Hotkey for moving cursor one word forward.
    pub move_forward_word: HotkeyBinding,
    /// Hotkey for deleting one character backward.
    pub delete_backward_char: HotkeyBinding,
    /// Hotkey for deleting one character forward.
    pub delete_forward_char: HotkeyBinding,
    /// Hotkey for deleting one word backward.
    pub delete_backward_word: HotkeyBinding,
    /// Hotkey for killing text to end of line.
    pub kill_to_end_of_line: HotkeyBinding,
    /// Hotkey for killing text to beginning of line.
    pub kill_to_beginning_of_line: HotkeyBinding,
    /// Hotkey for yanking (pasting) killed text.
    pub yank: HotkeyBinding,
    /// Hotkey for searching prompt history (reverse search).
    pub history_search: HotkeyBinding,
    /// Hotkey for toggling the pinned plan drawer.
    pub toggle_plan_drawer: HotkeyBinding,
    /// Hotkey for repainting the screen.
    pub redraw: HotkeyBinding,
    /// Hotkey for switching to the next model the agent offers.
    pub next_model: HotkeyBinding,
    /// Hotkey for switching to the previous model the agent offers.
    pub previous_model: HotkeyBinding,
    /// Hotkey for stepping to the next effort level the agent offers.
    pub next_effort: HotkeyBinding,
    /// Hotkey for stepping to the previous effort level the agent offers.
    pub previous_effort: HotkeyBinding,
    /// Hotkey for having an agent commit the working tree's changes.
    pub commit_changes: HotkeyBinding,
    /// Hotkey for opening the subagent switcher.
    pub switch_subagent: HotkeyBinding,
    /// Hotkey for stepping to the next model mode the agent offers.
    pub next_model_mode: HotkeyBinding,
    /// Hotkey for stepping to the previous model mode the agent offers.
    pub previous_model_mode: HotkeyBinding,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            open_transcript: HotkeyBinding::from_str(
                HotkeyAction::OpenTranscript.default_binding(),
            ),
            open_editor: HotkeyBinding::from_str(HotkeyAction::OpenEditor.default_binding()),
            move_backward_char: HotkeyBinding::from_str(
                HotkeyAction::MoveBackwardChar.default_binding(),
            ),
            move_forward_char: HotkeyBinding::from_str(
                HotkeyAction::MoveForwardChar.default_binding(),
            ),
            move_beginning_of_line: HotkeyBinding::from_str(
                HotkeyAction::MoveBeginningOfLine.default_binding(),
            ),
            move_end_of_line: HotkeyBinding::from_str(
                HotkeyAction::MoveEndOfLine.default_binding(),
            ),
            move_backward_word: HotkeyBinding::from_str(
                HotkeyAction::MoveBackwardWord.default_binding(),
            ),
            move_forward_word: HotkeyBinding::from_str(
                HotkeyAction::MoveForwardWord.default_binding(),
            ),
            delete_backward_char: HotkeyBinding::from_str(
                HotkeyAction::DeleteBackwardChar.default_binding(),
            ),
            delete_forward_char: HotkeyBinding::from_str(
                HotkeyAction::DeleteForwardChar.default_binding(),
            ),
            delete_backward_word: HotkeyBinding::from_str(
                HotkeyAction::DeleteBackwardWord.default_binding(),
            ),
            kill_to_end_of_line: HotkeyBinding::from_str(
                HotkeyAction::KillToEndOfLine.default_binding(),
            ),
            kill_to_beginning_of_line: HotkeyBinding::from_str(
                HotkeyAction::KillToBeginningOfLine.default_binding(),
            ),
            yank: HotkeyBinding::from_str(HotkeyAction::Yank.default_binding()),
            history_search: HotkeyBinding::from_str(HotkeyAction::HistorySearch.default_binding()),
            toggle_plan_drawer: HotkeyBinding::from_str(
                HotkeyAction::TogglePlanDrawer.default_binding(),
            ),
            redraw: HotkeyBinding::from_str(HotkeyAction::Redraw.default_binding()),
            next_model: HotkeyBinding::from_str(HotkeyAction::NextModel.default_binding()),
            previous_model: HotkeyBinding::from_str(HotkeyAction::PreviousModel.default_binding()),
            next_effort: HotkeyBinding::from_str(HotkeyAction::NextEffort.default_binding()),
            previous_effort: HotkeyBinding::from_str(
                HotkeyAction::PreviousEffort.default_binding(),
            ),
            commit_changes: HotkeyBinding::from_str(HotkeyAction::CommitChanges.default_binding()),
            switch_subagent: HotkeyBinding::from_str(
                HotkeyAction::SwitchSubagent.default_binding(),
            ),
            next_model_mode: HotkeyBinding::from_str(HotkeyAction::NextModelMode.default_binding()),
            previous_model_mode: HotkeyBinding::from_str(
                HotkeyAction::PreviousModelMode.default_binding(),
            ),
        }
    }
}

impl HotkeyConfig {
    /// Resolve from TOML config, applying defaults for missing values.
    pub fn from_toml(toml: &HotkeyConfigToml) -> Self {
        let defaults = Self::default();
        Self {
            open_transcript: toml
                .open_transcript
                .clone()
                .unwrap_or(defaults.open_transcript),
            open_editor: toml.open_editor.clone().unwrap_or(defaults.open_editor),
            move_backward_char: toml
                .move_backward_char
                .clone()
                .unwrap_or(defaults.move_backward_char),
            move_forward_char: toml
                .move_forward_char
                .clone()
                .unwrap_or(defaults.move_forward_char),
            move_beginning_of_line: toml
                .move_beginning_of_line
                .clone()
                .unwrap_or(defaults.move_beginning_of_line),
            move_end_of_line: toml
                .move_end_of_line
                .clone()
                .unwrap_or(defaults.move_end_of_line),
            move_backward_word: toml
                .move_backward_word
                .clone()
                .unwrap_or(defaults.move_backward_word),
            move_forward_word: toml
                .move_forward_word
                .clone()
                .unwrap_or(defaults.move_forward_word),
            delete_backward_char: toml
                .delete_backward_char
                .clone()
                .unwrap_or(defaults.delete_backward_char),
            delete_forward_char: toml
                .delete_forward_char
                .clone()
                .unwrap_or(defaults.delete_forward_char),
            delete_backward_word: toml
                .delete_backward_word
                .clone()
                .unwrap_or(defaults.delete_backward_word),
            kill_to_end_of_line: toml
                .kill_to_end_of_line
                .clone()
                .unwrap_or(defaults.kill_to_end_of_line),
            kill_to_beginning_of_line: toml
                .kill_to_beginning_of_line
                .clone()
                .unwrap_or(defaults.kill_to_beginning_of_line),
            yank: toml.yank.clone().unwrap_or(defaults.yank),
            history_search: toml
                .history_search
                .clone()
                .unwrap_or(defaults.history_search),
            toggle_plan_drawer: toml
                .toggle_plan_drawer
                .clone()
                .unwrap_or(defaults.toggle_plan_drawer),
            redraw: toml.redraw.clone().unwrap_or(defaults.redraw),
            next_model: toml.next_model.clone().unwrap_or(defaults.next_model),
            previous_model: toml
                .previous_model
                .clone()
                .unwrap_or(defaults.previous_model),
            next_effort: toml.next_effort.clone().unwrap_or(defaults.next_effort),
            previous_effort: toml
                .previous_effort
                .clone()
                .unwrap_or(defaults.previous_effort),
            commit_changes: toml
                .commit_changes
                .clone()
                .unwrap_or(defaults.commit_changes),
            switch_subagent: toml
                .switch_subagent
                .clone()
                .unwrap_or(defaults.switch_subagent),
            next_model_mode: toml
                .next_model_mode
                .clone()
                .unwrap_or(defaults.next_model_mode),
            previous_model_mode: toml
                .previous_model_mode
                .clone()
                .unwrap_or(defaults.previous_model_mode),
        }
    }

    /// Get the binding for a given action.
    pub fn binding_for(&self, action: HotkeyAction) -> &HotkeyBinding {
        match action {
            HotkeyAction::OpenTranscript => &self.open_transcript,
            HotkeyAction::OpenEditor => &self.open_editor,
            HotkeyAction::MoveBackwardChar => &self.move_backward_char,
            HotkeyAction::MoveForwardChar => &self.move_forward_char,
            HotkeyAction::MoveBeginningOfLine => &self.move_beginning_of_line,
            HotkeyAction::MoveEndOfLine => &self.move_end_of_line,
            HotkeyAction::MoveBackwardWord => &self.move_backward_word,
            HotkeyAction::MoveForwardWord => &self.move_forward_word,
            HotkeyAction::DeleteBackwardChar => &self.delete_backward_char,
            HotkeyAction::DeleteForwardChar => &self.delete_forward_char,
            HotkeyAction::DeleteBackwardWord => &self.delete_backward_word,
            HotkeyAction::KillToEndOfLine => &self.kill_to_end_of_line,
            HotkeyAction::KillToBeginningOfLine => &self.kill_to_beginning_of_line,
            HotkeyAction::Yank => &self.yank,
            HotkeyAction::HistorySearch => &self.history_search,
            HotkeyAction::TogglePlanDrawer => &self.toggle_plan_drawer,
            HotkeyAction::Redraw => &self.redraw,
            HotkeyAction::NextModel => &self.next_model,
            HotkeyAction::PreviousModel => &self.previous_model,
            HotkeyAction::NextEffort => &self.next_effort,
            HotkeyAction::PreviousEffort => &self.previous_effort,
            HotkeyAction::CommitChanges => &self.commit_changes,
            HotkeyAction::SwitchSubagent => &self.switch_subagent,
            HotkeyAction::NextModelMode => &self.next_model_mode,
            HotkeyAction::PreviousModelMode => &self.previous_model_mode,
        }
    }

    /// Set the binding for a given action.
    pub fn set_binding(&mut self, action: HotkeyAction, binding: HotkeyBinding) {
        match action {
            HotkeyAction::OpenTranscript => self.open_transcript = binding,
            HotkeyAction::OpenEditor => self.open_editor = binding,
            HotkeyAction::MoveBackwardChar => self.move_backward_char = binding,
            HotkeyAction::MoveForwardChar => self.move_forward_char = binding,
            HotkeyAction::MoveBeginningOfLine => self.move_beginning_of_line = binding,
            HotkeyAction::MoveEndOfLine => self.move_end_of_line = binding,
            HotkeyAction::MoveBackwardWord => self.move_backward_word = binding,
            HotkeyAction::MoveForwardWord => self.move_forward_word = binding,
            HotkeyAction::DeleteBackwardChar => self.delete_backward_char = binding,
            HotkeyAction::DeleteForwardChar => self.delete_forward_char = binding,
            HotkeyAction::DeleteBackwardWord => self.delete_backward_word = binding,
            HotkeyAction::KillToEndOfLine => self.kill_to_end_of_line = binding,
            HotkeyAction::KillToBeginningOfLine => self.kill_to_beginning_of_line = binding,
            HotkeyAction::Yank => self.yank = binding,
            HotkeyAction::HistorySearch => self.history_search = binding,
            HotkeyAction::TogglePlanDrawer => self.toggle_plan_drawer = binding,
            HotkeyAction::Redraw => self.redraw = binding,
            HotkeyAction::NextModel => self.next_model = binding,
            HotkeyAction::PreviousModel => self.previous_model = binding,
            HotkeyAction::NextEffort => self.next_effort = binding,
            HotkeyAction::PreviousEffort => self.previous_effort = binding,
            HotkeyAction::CommitChanges => self.commit_changes = binding,
            HotkeyAction::SwitchSubagent => self.switch_subagent = binding,
            HotkeyAction::NextModelMode => self.next_model_mode = binding,
            HotkeyAction::PreviousModelMode => self.previous_model_mode = binding,
        }
    }

    /// Return all (action, binding) pairs.
    pub fn all_bindings(&self) -> Vec<(HotkeyAction, &HotkeyBinding)> {
        vec![
            (HotkeyAction::OpenTranscript, &self.open_transcript),
            (HotkeyAction::OpenEditor, &self.open_editor),
            (HotkeyAction::MoveBackwardChar, &self.move_backward_char),
            (HotkeyAction::MoveForwardChar, &self.move_forward_char),
            (
                HotkeyAction::MoveBeginningOfLine,
                &self.move_beginning_of_line,
            ),
            (HotkeyAction::MoveEndOfLine, &self.move_end_of_line),
            (HotkeyAction::MoveBackwardWord, &self.move_backward_word),
            (HotkeyAction::MoveForwardWord, &self.move_forward_word),
            (HotkeyAction::DeleteBackwardChar, &self.delete_backward_char),
            (HotkeyAction::DeleteForwardChar, &self.delete_forward_char),
            (HotkeyAction::DeleteBackwardWord, &self.delete_backward_word),
            (HotkeyAction::KillToEndOfLine, &self.kill_to_end_of_line),
            (
                HotkeyAction::KillToBeginningOfLine,
                &self.kill_to_beginning_of_line,
            ),
            (HotkeyAction::Yank, &self.yank),
            (HotkeyAction::HistorySearch, &self.history_search),
            (HotkeyAction::TogglePlanDrawer, &self.toggle_plan_drawer),
            (HotkeyAction::Redraw, &self.redraw),
            (HotkeyAction::NextModel, &self.next_model),
            (HotkeyAction::PreviousModel, &self.previous_model),
            (HotkeyAction::NextEffort, &self.next_effort),
            (HotkeyAction::PreviousEffort, &self.previous_effort),
            (HotkeyAction::CommitChanges, &self.commit_changes),
            (HotkeyAction::SwitchSubagent, &self.switch_subagent),
            (HotkeyAction::NextModelMode, &self.next_model_mode),
            (HotkeyAction::PreviousModelMode, &self.previous_model_mode),
        ]
    }
}

// ============================================================================
// Footer Segment Configuration
// ============================================================================

/// Individual footer segments that can be enabled/disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FooterSegment {
    /// Task summary: "Task: <summary>"
    PromptSummary,
    /// Agent-supplied session title: "Title: Fix login flakes"
    SessionTitle,
    /// Vim mode indicator: "NORMAL" or "INSERT"
    VimMode,
    /// Git branch: "⎇ branch-name"
    GitBranch,
    /// Worktree name: "Worktree: name"
    WorktreeName,
    /// Git stats: "+10 -3"
    GitStats,
    /// Default context window display: "27% / 128k"
    Context,
    /// Percentage of the context window currently used: "27%"
    ContextUsedPercent,
    /// Percentage of the context window still available: "73%"
    ContextRemainingPercent,
    /// Tokens currently used in the context window: "34.0k"
    ContextUsedTokens,
    /// Tokens still available in the context window: "94.0k"
    ContextRemainingTokens,
    /// Maximum context window size: "128k"
    ContextWindowTokens,
    /// Approval mode: "Approvals: Agent"
    ApprovalMode,
    /// Active skillset: "Skillset: name"
    Skillset,
    /// Nori version: "Skillsets v19.1.1"
    NoriVersion,
    /// Token usage: "Tokens: 77K total (32K cached)"
    TokenUsage,
    /// Model the agent is using: "Opus 5.5". Hidden when the agent offers no model choice.
    Model,
    /// ACP mode indicator: "[ Plan ]"
    ModeIndicator,
    /// Cloud session identity: "☁ nori-fast-kazunoko-aac8". Only rendered when
    /// attached to a cloud-mode session; self-hides otherwise.
    CloudSession,
}

impl FooterSegment {
    /// Human-readable name for display in the TUI.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::PromptSummary => "Task Summary",
            Self::SessionTitle => "Session Title",
            Self::VimMode => "Vim Mode",
            Self::GitBranch => "Git Branch",
            Self::WorktreeName => "Worktree Name",
            Self::GitStats => "Git Stats",
            Self::Context => "Context Window",
            Self::ContextUsedPercent => "Context Used %",
            Self::ContextRemainingPercent => "Context Remaining %",
            Self::ContextUsedTokens => "Context Used Tokens",
            Self::ContextRemainingTokens => "Context Remaining Tokens",
            Self::ContextWindowTokens => "Context Window Tokens",
            Self::ApprovalMode => "Approvals",
            Self::Skillset => "Skillset",
            Self::NoriVersion => "Skillset Version",
            Self::TokenUsage => "Token Usage",
            Self::Model => "Model",
            Self::ModeIndicator => "Mode Indicator",
            Self::CloudSession => "Cloud Session",
        }
    }

    /// The TOML key name for this segment under `[tui.footer_segments]`.
    pub fn toml_key(&self) -> &'static str {
        match self {
            Self::PromptSummary => "prompt_summary",
            Self::SessionTitle => "session_title",
            Self::VimMode => "vim_mode",
            Self::GitBranch => "git_branch",
            Self::WorktreeName => "worktree_name",
            Self::GitStats => "git_stats",
            Self::Context => "context",
            Self::ContextUsedPercent => "context_used_percent",
            Self::ContextRemainingPercent => "context_remaining_percent",
            Self::ContextUsedTokens => "context_used_tokens",
            Self::ContextRemainingTokens => "context_remaining_tokens",
            Self::ContextWindowTokens => "context_window_tokens",
            Self::ApprovalMode => "approval_mode",
            Self::Skillset => "skillset",
            Self::NoriVersion => "nori_version",
            Self::TokenUsage => "token_usage",
            Self::Model => "model",
            Self::ModeIndicator => "mode_indicator",
            Self::CloudSession => "cloud_session",
        }
    }

    /// All footer segment variants, in display order.
    pub fn all_variants() -> &'static [FooterSegment] {
        &[
            Self::PromptSummary,
            Self::SessionTitle,
            Self::VimMode,
            Self::GitBranch,
            Self::WorktreeName,
            Self::GitStats,
            Self::Context,
            Self::ContextUsedPercent,
            Self::ContextRemainingPercent,
            Self::ContextUsedTokens,
            Self::ContextRemainingTokens,
            Self::ContextWindowTokens,
            Self::ApprovalMode,
            Self::Skillset,
            Self::NoriVersion,
            Self::TokenUsage,
            Self::Model,
            Self::ModeIndicator,
            Self::CloudSession,
        ]
    }

    /// Default order of footer segments (same as all_variants).
    pub fn default_order() -> &'static [FooterSegment] {
        Self::all_variants()
    }

    fn from_toml_key(key: &str) -> Option<Self> {
        Self::all_variants()
            .iter()
            .copied()
            .find(|segment| segment.toml_key() == key)
    }
}

impl fmt::Display for FooterSegment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// TOML-deserializable footer segment configuration.
/// Each field is optional; unspecified fields use `FooterSegmentConfig::default`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FooterSegmentConfigToml {
    /// Enable/disable task summary segment.
    pub prompt_summary: Option<bool>,
    /// Enable/disable agent-supplied session title segment.
    pub session_title: Option<bool>,
    /// Enable/disable vim mode indicator.
    pub vim_mode: Option<bool>,
    /// Enable/disable git branch segment.
    pub git_branch: Option<bool>,
    /// Enable/disable worktree name segment.
    pub worktree_name: Option<bool>,
    /// Enable/disable git stats segment.
    pub git_stats: Option<bool>,
    /// Enable/disable context window segment.
    pub context: Option<bool>,
    /// Enable/disable context used percentage segment.
    pub context_used_percent: Option<bool>,
    /// Enable/disable context remaining percentage segment.
    pub context_remaining_percent: Option<bool>,
    /// Enable/disable context used token segment.
    pub context_used_tokens: Option<bool>,
    /// Enable/disable context remaining token segment.
    pub context_remaining_tokens: Option<bool>,
    /// Enable/disable context window maximum token segment.
    pub context_window_tokens: Option<bool>,
    /// Enable/disable approval mode segment.
    pub approval_mode: Option<bool>,
    /// Enable/disable active skillset segment.
    pub skillset: Option<bool>,
    /// Enable/disable nori version segment.
    pub nori_version: Option<bool>,
    /// Enable/disable token usage segment.
    pub token_usage: Option<bool>,
    /// Enable/disable current model segment.
    pub model: Option<bool>,
    /// Enable/disable ACP mode indicator segment.
    pub mode_indicator: Option<bool>,
    /// Enable/disable cloud session identity segment.
    pub cloud_session: Option<bool>,
}

/// Resolved footer segment configuration with defaults applied.
#[derive(Debug, Clone)]
pub struct FooterSegmentConfig {
    /// Enable/disable task summary segment.
    pub prompt_summary: bool,
    /// Enable/disable agent-supplied session title segment.
    pub session_title: bool,
    /// Enable/disable vim mode indicator.
    pub vim_mode: bool,
    /// Enable/disable git branch segment.
    pub git_branch: bool,
    /// Enable/disable worktree name segment.
    pub worktree_name: bool,
    /// Enable/disable git stats segment.
    pub git_stats: bool,
    /// Enable/disable context window segment.
    pub context: bool,
    /// Enable/disable context used percentage segment.
    pub context_used_percent: bool,
    /// Enable/disable context remaining percentage segment.
    pub context_remaining_percent: bool,
    /// Enable/disable context used token segment.
    pub context_used_tokens: bool,
    /// Enable/disable context remaining token segment.
    pub context_remaining_tokens: bool,
    /// Enable/disable context window maximum token segment.
    pub context_window_tokens: bool,
    /// Enable/disable approval mode segment.
    pub approval_mode: bool,
    /// Enable/disable active skillset segment.
    pub skillset: bool,
    /// Enable/disable nori version segment.
    pub nori_version: bool,
    /// Enable/disable token usage segment.
    pub token_usage: bool,
    /// Enable/disable current model segment.
    pub model: bool,
    /// Enable/disable ACP mode indicator segment.
    pub mode_indicator: bool,
    /// Enable/disable cloud session identity segment.
    pub cloud_session: bool,
}

impl Default for FooterSegmentConfig {
    fn default() -> Self {
        // Quiet defaults: an idle local shell shows where you are (branch,
        // worktree), how much room is left (context), and which agent mode is
        // active. Everything else is either self-hiding state that only
        // appears when it is true (cloud session, vim mode) or off, because it
        // is static, restates the transcript, or belongs in `/status`:
        // approvals, skillset, skillset version, session title, and cumulative
        // token usage. Each stays one `[tui.footer_segments]` line away.
        Self {
            prompt_summary: false,
            session_title: false,
            vim_mode: true,
            git_branch: true,
            worktree_name: true,
            git_stats: false,
            context: true,
            context_used_percent: false,
            context_remaining_percent: false,
            context_used_tokens: false,
            context_remaining_tokens: false,
            context_window_tokens: false,
            approval_mode: false,
            skillset: false,
            nori_version: false,
            token_usage: false,
            model: true,
            mode_indicator: true,
            cloud_session: true,
        }
    }
}

impl FooterSegmentConfig {
    /// Resolve from TOML config, applying defaults for missing values.
    pub fn from_toml(toml: &FooterSegmentConfigToml) -> Self {
        let defaults = Self::default();
        Self {
            prompt_summary: toml.prompt_summary.unwrap_or(defaults.prompt_summary),
            session_title: toml.session_title.unwrap_or(defaults.session_title),
            vim_mode: toml.vim_mode.unwrap_or(defaults.vim_mode),
            git_branch: toml.git_branch.unwrap_or(defaults.git_branch),
            worktree_name: toml.worktree_name.unwrap_or(defaults.worktree_name),
            git_stats: toml.git_stats.unwrap_or(defaults.git_stats),
            context: toml.context.unwrap_or(defaults.context),
            context_used_percent: toml
                .context_used_percent
                .unwrap_or(defaults.context_used_percent),
            context_remaining_percent: toml
                .context_remaining_percent
                .unwrap_or(defaults.context_remaining_percent),
            context_used_tokens: toml
                .context_used_tokens
                .unwrap_or(defaults.context_used_tokens),
            context_remaining_tokens: toml
                .context_remaining_tokens
                .unwrap_or(defaults.context_remaining_tokens),
            context_window_tokens: toml
                .context_window_tokens
                .unwrap_or(defaults.context_window_tokens),
            approval_mode: toml.approval_mode.unwrap_or(defaults.approval_mode),
            skillset: toml.skillset.unwrap_or(defaults.skillset),
            nori_version: toml.nori_version.unwrap_or(defaults.nori_version),
            token_usage: toml.token_usage.unwrap_or(defaults.token_usage),
            model: toml.model.unwrap_or(defaults.model),
            mode_indicator: toml.mode_indicator.unwrap_or(defaults.mode_indicator),
            cloud_session: toml.cloud_session.unwrap_or(defaults.cloud_session),
        }
    }

    /// Check if a segment is enabled.
    pub fn is_enabled(&self, segment: FooterSegment) -> bool {
        match segment {
            FooterSegment::PromptSummary => self.prompt_summary,
            FooterSegment::SessionTitle => self.session_title,
            FooterSegment::VimMode => self.vim_mode,
            FooterSegment::GitBranch => self.git_branch,
            FooterSegment::WorktreeName => self.worktree_name,
            FooterSegment::GitStats => self.git_stats,
            FooterSegment::Context => self.context,
            FooterSegment::ContextUsedPercent => self.context_used_percent,
            FooterSegment::ContextRemainingPercent => self.context_remaining_percent,
            FooterSegment::ContextUsedTokens => self.context_used_tokens,
            FooterSegment::ContextRemainingTokens => self.context_remaining_tokens,
            FooterSegment::ContextWindowTokens => self.context_window_tokens,
            FooterSegment::ApprovalMode => self.approval_mode,
            FooterSegment::Skillset => self.skillset,
            FooterSegment::NoriVersion => self.nori_version,
            FooterSegment::TokenUsage => self.token_usage,
            FooterSegment::Model => self.model,
            FooterSegment::ModeIndicator => self.mode_indicator,
            FooterSegment::CloudSession => self.cloud_session,
        }
    }

    /// Set whether a segment is enabled.
    pub fn set_enabled(&mut self, segment: FooterSegment, enabled: bool) {
        match segment {
            FooterSegment::PromptSummary => self.prompt_summary = enabled,
            FooterSegment::SessionTitle => self.session_title = enabled,
            FooterSegment::VimMode => self.vim_mode = enabled,
            FooterSegment::GitBranch => self.git_branch = enabled,
            FooterSegment::WorktreeName => self.worktree_name = enabled,
            FooterSegment::GitStats => self.git_stats = enabled,
            FooterSegment::Context => self.context = enabled,
            FooterSegment::ContextUsedPercent => self.context_used_percent = enabled,
            FooterSegment::ContextRemainingPercent => self.context_remaining_percent = enabled,
            FooterSegment::ContextUsedTokens => self.context_used_tokens = enabled,
            FooterSegment::ContextRemainingTokens => self.context_remaining_tokens = enabled,
            FooterSegment::ContextWindowTokens => self.context_window_tokens = enabled,
            FooterSegment::ApprovalMode => self.approval_mode = enabled,
            FooterSegment::Skillset => self.skillset = enabled,
            FooterSegment::NoriVersion => self.nori_version = enabled,
            FooterSegment::TokenUsage => self.token_usage = enabled,
            FooterSegment::Model => self.model = enabled,
            FooterSegment::ModeIndicator => self.mode_indicator = enabled,
            FooterSegment::CloudSession => self.cloud_session = enabled,
        }
    }

    /// Return all (segment, enabled) pairs in default order.
    pub fn all_settings(&self) -> Vec<(FooterSegment, bool)> {
        FooterSegment::all_variants()
            .iter()
            .map(|s| (*s, self.is_enabled(*s)))
            .collect()
    }
}

/// A footer layout entry: either one built-in segment or one custom format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterLayoutItem {
    Builtin(FooterSegment),
    Custom(FooterFormat),
}

impl From<FooterSegment> for FooterLayoutItem {
    fn from(segment: FooterSegment) -> Self {
        Self::Builtin(segment)
    }
}

impl<'de> Deserialize<'de> for FooterLayoutItem {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct CustomFooterFormat {
            format: String,
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum FooterLayoutItemToml {
            Builtin(String),
            Custom(CustomFooterFormat),
        }

        match FooterLayoutItemToml::deserialize(deserializer)? {
            FooterLayoutItemToml::Builtin(key) => FooterSegment::from_toml_key(&key)
                .map(Self::Builtin)
                .ok_or_else(|| serde::de::Error::custom(format!("unknown footer segment `{key}`"))),
            FooterLayoutItemToml::Custom(custom) => FooterFormat::parse(&custom.format)
                .map(Self::Custom)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// A validated custom footer format, compiled when configuration is loaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterFormat {
    parts: Vec<FooterFormatPart>,
}

impl FooterFormat {
    fn parse(format: &str) -> Result<Self, String> {
        let mut chars = format.chars().peekable();
        let mut parts = Vec::new();
        let mut text = String::new();

        while let Some(ch) = chars.next() {
            match ch {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    text.push('{');
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    text.push('}');
                }
                '{' => {
                    if !text.is_empty() {
                        parts.push(FooterFormatPart::Text(std::mem::take(&mut text)));
                    }

                    let mut placeholder = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            Some('{') => {
                                return Err(format!(
                                    "invalid footer format `{format}`: nested `{{` in placeholder"
                                ));
                            }
                            Some(ch) => placeholder.push(ch),
                            None => {
                                return Err(format!(
                                    "invalid footer format `{format}`: missing closing `}}`"
                                ));
                            }
                        }
                    }

                    let Some(segment) = FooterSegment::from_toml_key(&placeholder) else {
                        return Err(format!(
                            "unknown footer segment placeholder `{placeholder}` in `{format}`"
                        ));
                    };
                    parts.push(FooterFormatPart::Segment(segment));
                }
                '}' => {
                    return Err(format!(
                        "invalid footer format `{format}`: unmatched closing `}}`"
                    ));
                }
                _ => text.push(ch),
            }
        }

        if !text.is_empty() {
            parts.push(FooterFormatPart::Text(text));
        }

        Ok(Self { parts })
    }

    pub fn parts(&self) -> &[FooterFormatPart] {
        &self.parts
    }
}

/// One compiled piece of a custom footer format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FooterFormatPart {
    Text(String),
    Segment(FooterSegment),
}

/// TOML-deserializable footer segment placement settings.
///
/// Each field replaces that placement when present. Listed segments are moved
/// out of other default placements so a partial override like
/// `textarea_top_right = ["mode_indicator"]` moves the mode indicator instead
/// of duplicating it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct FooterLayoutConfigToml {
    pub footer_left: Option<Vec<FooterLayoutItem>>,
    pub footer_right: Option<Vec<FooterLayoutItem>>,
    pub textarea_top_left: Option<Vec<FooterLayoutItem>>,
    pub textarea_top_right: Option<Vec<FooterLayoutItem>>,
    pub textarea_bottom_left: Option<Vec<FooterLayoutItem>>,
    pub textarea_bottom_right: Option<Vec<FooterLayoutItem>>,
}

/// Resolved footer segment placement configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FooterLayoutConfig {
    pub footer_left: Vec<FooterLayoutItem>,
    pub footer_right: Vec<FooterLayoutItem>,
    pub textarea_top_left: Vec<FooterLayoutItem>,
    pub textarea_top_right: Vec<FooterLayoutItem>,
    pub textarea_bottom_left: Vec<FooterLayoutItem>,
    pub textarea_bottom_right: Vec<FooterLayoutItem>,
}

impl Default for FooterLayoutConfig {
    fn default() -> Self {
        Self {
            // Consolidate session metadata onto one clean Helix-style bottom
            // statusline; textarea corners remain available for user opt-in.
            footer_left: vec![
                FooterSegment::VimMode.into(),
                FooterSegment::GitBranch.into(),
                FooterSegment::WorktreeName.into(),
                FooterSegment::CloudSession.into(),
                FooterSegment::PromptSummary.into(),
                FooterSegment::SessionTitle.into(),
                FooterSegment::GitStats.into(),
                FooterSegment::ContextUsedPercent.into(),
                FooterSegment::ContextRemainingPercent.into(),
                FooterSegment::ContextUsedTokens.into(),
                FooterSegment::ContextRemainingTokens.into(),
                FooterSegment::ContextWindowTokens.into(),
                FooterSegment::ApprovalMode.into(),
                FooterSegment::Skillset.into(),
                FooterSegment::NoriVersion.into(),
                FooterSegment::TokenUsage.into(),
            ],
            footer_right: vec![
                FooterSegment::Model.into(),
                FooterSegment::ModeIndicator.into(),
                FooterSegment::Context.into(),
            ],
            textarea_top_left: Vec::new(),
            textarea_top_right: Vec::new(),
            textarea_bottom_left: Vec::new(),
            textarea_bottom_right: Vec::new(),
        }
    }
}

impl FooterLayoutConfig {
    pub fn from_toml(toml: &FooterLayoutConfigToml) -> Self {
        let mut config = Self::default();

        if let Some(segments) = &toml.footer_left {
            config.remove_segments(segments);
            config.footer_left = segments.clone();
        }
        if let Some(segments) = &toml.footer_right {
            config.remove_segments(segments);
            config.footer_right = segments.clone();
        }
        if let Some(segments) = &toml.textarea_top_left {
            config.remove_segments(segments);
            config.textarea_top_left = segments.clone();
        }
        if let Some(segments) = &toml.textarea_top_right {
            config.remove_segments(segments);
            config.textarea_top_right = segments.clone();
        }
        if let Some(segments) = &toml.textarea_bottom_left {
            config.remove_segments(segments);
            config.textarea_bottom_left = segments.clone();
        }
        if let Some(segments) = &toml.textarea_bottom_right {
            config.remove_segments(segments);
            config.textarea_bottom_right = segments.clone();
        }

        config
    }

    fn remove_segments(&mut self, segments: &[FooterLayoutItem]) {
        self.footer_left
            .retain(|segment| !segments.contains(segment));
        self.footer_right
            .retain(|segment| !segments.contains(segment));
        self.textarea_top_left
            .retain(|segment| !segments.contains(segment));
        self.textarea_top_right
            .retain(|segment| !segments.contains(segment));
        self.textarea_bottom_left
            .retain(|segment| !segments.contains(segment));
        self.textarea_bottom_right
            .retain(|segment| !segments.contains(segment));
    }
}

/// TUI-specific settings (TOML)
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct TuiConfigToml {
    /// Enable animations (shimmer effects, spinners)
    pub animations: Option<bool>,

    /// Legacy and ignored: the stage always lays the conversation out again at a new width.
    /// Kept so config files that still have it load.
    pub resize_reflow: Option<bool>,

    /// Terminal notification preference (OSC 9 escape sequences)
    pub terminal_notifications: Option<TerminalNotifications>,

    /// OS-level desktop notification preference (notify-rust)
    pub os_notifications: Option<OsNotifications>,

    /// Stack footer segments vertically in the status footer.
    pub vertical_footer: Option<bool>,

    /// Show agent session updates as brief toasts in the top right instead of in the history.
    /// On by default; set to false to show nothing at all.
    pub toasts: Option<bool>,

    /// How long after idle before sending a notification.
    pub notify_after_idle: Option<NotifyAfterIdle>,

    /// Vim mode and Enter key behavior. Accepts `true`/`false` for backwards
    /// compatibility or one of `"newline"`, `"submit"`, `"off"`.
    pub vim_mode: Option<VimEnterBehavior>,

    /// Prompt editor: `kakoune` (default), its `helix` alias, `vim`, or `standard`.
    pub editor_mode: Option<String>,

    /// Configurable hotkey bindings.
    #[serde(default)]
    pub hotkeys: HotkeyConfigToml,

    /// Rebinds of the keys the TUI answers itself, by id (see the `keymap` module).
    #[serde(default)]
    pub keymap: std::collections::BTreeMap<String, crate::keymap::KeyOverride>,

    /// Footer segment visibility settings.
    #[serde(default)]
    pub footer_segments: FooterSegmentConfigToml,

    /// Footer segment placement settings.
    #[serde(default)]
    pub footer_layout: FooterLayoutConfigToml,

    /// Timeout for custom prompt script execution.
    pub script_timeout: Option<ScriptTimeout>,

    /// Number of times to re-run the first prompt in fresh sessions.
    /// `None` or absent means disabled.
    pub loop_count: Option<i32>,

    /// Automatically create a git worktree at session start.
    pub auto_worktree: Option<AutoWorktree>,

    /// Enable per-session skillset isolation.
    pub skillset_per_session: Option<bool>,

    /// Terminal file manager for the `/browse` command.
    pub file_manager: Option<FileManager>,

    /// Pin plan updates to a drawer in the viewport instead of history cells.
    pub pinned_plan_drawer: Option<bool>,

    /// Show rotating custom messages while the agent is working.
    pub custom_working_messages: Option<bool>,

    /// User-supplied list of working messages. When non-empty and
    /// `custom_working_messages` is enabled, the TUI samples from this list
    /// instead of the builtin whimsical messages.
    pub custom_working_message_list: Option<Vec<String>>,
}

/// Resolved TUI configuration
#[derive(Debug, Clone)]
pub struct TuiConfig {
    /// Enable animations (shimmer effects, spinners)
    pub animations: bool,

    /// Terminal notification preference (OSC 9 escape sequences)
    pub terminal_notifications: TerminalNotifications,

    /// OS-level desktop notification preference (notify-rust)
    pub os_notifications: OsNotifications,

    /// Stack footer segments vertically in the status footer.
    pub vertical_footer: bool,

    /// Show agent session updates as brief toasts in the top right.
    pub toasts: bool,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            animations: true,
            terminal_notifications: TerminalNotifications::Enabled,
            os_notifications: OsNotifications::Enabled,
            vertical_footer: false,
            toasts: true,
        }
    }
}

/// Supported terminal file managers for the `/browse` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileManager {
    /// vifm — chooser flag: `--choose-files <path>`
    Vifm,
    /// ranger — chooser flag: `--choosefile=<path>`
    Ranger,
    /// lf — chooser flag: `-selection-path <path>`
    Lf,
    /// nnn — chooser flag: `-p <path>`
    Nnn,
}

impl FileManager {
    /// Returns the binary name to invoke.
    pub fn command_name(self) -> &'static str {
        match self {
            Self::Vifm => "vifm",
            Self::Ranger => "ranger",
            Self::Lf => "lf",
            Self::Nnn => "nnn",
        }
    }

    /// Returns CLI arguments that put the file manager into chooser mode,
    /// writing the selected file path(s) to `output_path`.
    pub fn chooser_args(self, output_path: &std::path::Path) -> Vec<String> {
        let path = output_path.display().to_string();
        match self {
            Self::Vifm => vec!["--choose-files".to_string(), path],
            Self::Ranger => vec![format!("--choosefile={path}")],
            Self::Lf => vec!["-selection-path".to_string(), path],
            Self::Nnn => vec!["-p".to_string(), path],
        }
    }

    /// Human-friendly name shown in the config picker.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Vifm => "vifm",
            Self::Ranger => "ranger",
            Self::Lf => "lf",
            Self::Nnn => "nnn",
        }
    }
}

impl fmt::Display for FileManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// CLI overrides for config values
#[derive(Debug, Clone, Default)]
pub struct NoriConfigOverrides {
    /// Override the agent selection
    pub agent: Option<String>,

    /// Override sandbox mode
    pub sandbox_mode: Option<SandboxMode>,

    /// Override approval policy
    pub approval_policy: Option<AskForApproval>,

    /// Override current working directory
    pub cwd: Option<PathBuf>,

    /// Additional directories writable under the workspace-write sandbox.
    pub additional_writable_roots: Vec<PathBuf>,

    /// Dotted-path TOML overrides from `-c key=value` flags.
    pub raw_overrides: Vec<(String, toml::Value)>,
}

/// Trust settings resolved for the active project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ProjectConfig {
    pub trust_level: Option<TrustLevel>,
}

/// Resolved configuration with defaults applied
#[derive(Debug, Clone)]
pub struct NoriConfig {
    /// The ACP agent to use (e.g., "claude-code", "codex", "gemini")
    /// Persisted to track user's agent preference across sessions
    pub agent: String,

    /// The active ACP agent slug (CLI override > persisted agent)
    pub active_agent: String,

    /// Sandbox mode for command execution
    pub sandbox_mode: SandboxMode,

    /// Resolved sandbox policy for command execution.
    pub sandbox_policy: SandboxPolicy,

    /// Approval policy for commands
    pub approval_policy: AskForApproval,

    /// Whether approval or sandbox policy was explicitly configured.
    pub has_explicit_approval_or_sandbox_policy: bool,

    /// Whether workspace-write was downgraded because Windows sandboxing is unavailable.
    pub forced_auto_mode_downgraded_on_windows: bool,

    /// Whether the experimental Windows sandbox is enabled in user config.
    pub windows_sandbox_enabled: bool,

    /// Environment inherited by sandboxed commands.
    pub shell_environment_policy: ShellEnvironmentPolicy,

    /// Trust settings for the active working directory.
    pub active_project: ProjectConfig,

    /// User acknowledgement state for safety notices.
    pub notices: Notice,

    /// Whether to check for Nori updates at startup.
    pub check_for_update_on_startup: bool,

    /// Disable burst-paste detection in the prompt composer.
    pub disable_paste_burst: bool,

    /// History persistence policy
    pub history_persistence: HistoryPersistence,

    /// Which Chrome profile the `/browser` command launches against.
    pub browser_profile: BrowserProfileMode,

    /// External notifier command and arguments.
    pub notify: Option<Vec<String>>,

    /// Advertise subagent support to agents during `initialize`.
    pub subagent_sessions: bool,

    /// Give every session the nudox MCP server when `nudox` is installed.
    pub nudox: bool,

    /// ACP wire proxy logging settings.
    pub acp_proxy: AcpProxyConfig,

    /// Enable TUI animations
    pub animations: bool,

    /// Terminal notification preference (OSC 9 escape sequences)
    pub terminal_notifications: TerminalNotifications,

    /// OS-level desktop notification preference (notify-rust)
    pub os_notifications: OsNotifications,

    /// Stack footer segments vertically in the status footer.
    pub vertical_footer: bool,

    /// Show agent session updates as brief toasts in the top right.
    pub toasts: bool,

    /// How long after idle before sending a notification.
    pub notify_after_idle: NotifyAfterIdle,

    /// Vim mode and Enter key behavior.
    pub vim_mode: VimEnterBehavior,

    /// Whether to use Kakoune editing in the prompt.
    pub kakoune_mode: bool,

    /// Configurable hotkey bindings.
    pub hotkeys: HotkeyConfig,

    /// Every key the TUI answers itself, with the user's rebinds applied.
    pub keymap: crate::keymap::Keymap,

    /// Timeout for custom prompt script execution.
    pub script_timeout: ScriptTimeout,

    /// Number of times to re-run the first prompt in fresh sessions.
    /// `None` means disabled (default).
    pub loop_count: Option<i32>,

    /// Automatically create a git worktree at session start.
    pub auto_worktree: AutoWorktree,

    /// Enable per-session skillset isolation.
    pub skillset_per_session: bool,

    /// Terminal file manager for the `/browse` command.
    /// `None` means not configured.
    pub file_manager: Option<FileManager>,

    /// Pin plan updates to a drawer in the viewport instead of history cells.
    pub pinned_plan_drawer: bool,

    /// Show rotating custom messages while the agent is working.
    pub custom_working_messages: bool,

    /// Optional user-supplied list of working messages. When non-empty and
    /// `custom_working_messages` is `true`, the TUI samples from this list
    /// instead of the builtin whimsical messages.
    pub custom_working_message_list: Vec<String>,

    /// Footer segment visibility configuration.
    pub footer_segment_config: FooterSegmentConfig,

    /// Footer segment placement configuration.
    pub footer_layout_config: FooterLayoutConfig,

    /// Nori home directory (~/.nori/cli)
    pub nori_home: PathBuf,

    /// Current working directory
    pub cwd: PathBuf,

    /// MCP server configurations
    pub mcp_servers: HashMap<String, McpServerConfig>,

    /// Scripts to run when a session starts.
    pub session_start_hooks: Vec<PathBuf>,

    /// Scripts to run when a session ends.
    pub session_end_hooks: Vec<PathBuf>,

    /// Scripts to run before a user prompt is sent to the agent.
    pub pre_user_prompt_hooks: Vec<PathBuf>,

    /// Scripts to run after a user prompt is sent to the agent.
    pub post_user_prompt_hooks: Vec<PathBuf>,

    /// Scripts to run before a tool call is executed.
    pub pre_tool_call_hooks: Vec<PathBuf>,

    /// Scripts to run after a tool call completes.
    pub post_tool_call_hooks: Vec<PathBuf>,

    /// Scripts to run before the agent produces a response.
    pub pre_agent_response_hooks: Vec<PathBuf>,

    /// Scripts to run after the agent finishes its response.
    pub post_agent_response_hooks: Vec<PathBuf>,

    /// How changes are committed and proposed.
    pub git: GitConfig,

    /// Async (fire-and-forget) scripts to run when a session starts.
    pub async_session_start_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run when a session ends.
    pub async_session_end_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run before a user prompt is sent.
    pub async_pre_user_prompt_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run after a user prompt is sent.
    pub async_post_user_prompt_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run before a tool call is executed.
    pub async_pre_tool_call_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run after a tool call completes.
    pub async_post_tool_call_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run before the agent produces a response.
    pub async_pre_agent_response_hooks: Vec<PathBuf>,

    /// Async (fire-and-forget) scripts to run after the agent finishes its response.
    pub async_post_agent_response_hooks: Vec<PathBuf>,

    /// Default model overrides per agent (e.g., "claude-code" -> "haiku")
    pub default_models: HashMap<String, String>,

    /// Custom agent definitions from config
    pub agents: Vec<AgentConfigToml>,

    /// Cloud broker URL from config (e.g., "https://nori-broker.myorg.fly.dev")
    pub cloud_broker_url: Option<String>,
}

impl Default for NoriConfig {
    fn default() -> Self {
        Self {
            agent: DEFAULT_AGENT.to_string(),
            active_agent: DEFAULT_AGENT.to_string(),
            sandbox_mode: SandboxMode::DangerFullAccess,
            sandbox_policy: SandboxPolicy::DangerFullAccess,
            approval_policy: AskForApproval::Never,
            has_explicit_approval_or_sandbox_policy: false,
            forced_auto_mode_downgraded_on_windows: false,
            windows_sandbox_enabled: false,
            shell_environment_policy: ShellEnvironmentPolicy::default(),
            active_project: ProjectConfig::default(),
            notices: Notice::default(),
            check_for_update_on_startup: true,
            disable_paste_burst: false,
            history_persistence: HistoryPersistence::default(),
            browser_profile: BrowserProfileMode::default(),
            notify: None,
            subagent_sessions: true,
            nudox: true,
            acp_proxy: AcpProxyConfig {
                enabled: false,
                log_dir: PathBuf::from(".nori/cli/acp-wire"),
            },
            animations: true,
            terminal_notifications: TerminalNotifications::Enabled,
            os_notifications: OsNotifications::Enabled,
            vertical_footer: false,
            toasts: true,
            notify_after_idle: NotifyAfterIdle::default(),
            vim_mode: VimEnterBehavior::Off,
            kakoune_mode: true,
            hotkeys: HotkeyConfig::default(),
            keymap: crate::keymap::Keymap::default(),
            script_timeout: ScriptTimeout::default(),
            loop_count: None,
            auto_worktree: AutoWorktree::Off,
            skillset_per_session: false,
            file_manager: None,
            pinned_plan_drawer: false,
            custom_working_messages: true,
            custom_working_message_list: Vec::new(),
            footer_segment_config: FooterSegmentConfig::default(),
            footer_layout_config: FooterLayoutConfig::default(),
            nori_home: PathBuf::from(".nori/cli"),
            cwd: std::env::current_dir().unwrap_or_default(),
            mcp_servers: HashMap::new(),
            session_start_hooks: Vec::new(),
            session_end_hooks: Vec::new(),
            pre_user_prompt_hooks: Vec::new(),
            post_user_prompt_hooks: Vec::new(),
            pre_tool_call_hooks: Vec::new(),
            post_tool_call_hooks: Vec::new(),
            pre_agent_response_hooks: Vec::new(),
            post_agent_response_hooks: Vec::new(),
            git: GitConfig::default(),
            async_session_start_hooks: Vec::new(),
            async_session_end_hooks: Vec::new(),
            async_pre_user_prompt_hooks: Vec::new(),
            async_post_user_prompt_hooks: Vec::new(),
            async_pre_tool_call_hooks: Vec::new(),
            async_post_tool_call_hooks: Vec::new(),
            async_pre_agent_response_hooks: Vec::new(),
            async_post_agent_response_hooks: Vec::new(),
            default_models: HashMap::new(),
            agents: Vec::new(),
            cloud_broker_url: None,
        }
    }
}

impl NoriConfig {
    /// Downgrade workspace-write when the Windows sandbox is unavailable.
    pub fn apply_windows_sandbox_availability(&mut self, sandbox_available: bool) {
        let needs_downgrade = !sandbox_available
            && matches!(self.sandbox_policy, SandboxPolicy::WorkspaceWrite { .. });
        self.forced_auto_mode_downgraded_on_windows = needs_downgrade;
        if needs_downgrade {
            self.sandbox_policy = SandboxPolicy::new_read_only_policy();
        }
    }
}

// ============================================================================
// Session Hooks Configuration
// ============================================================================

/// TOML-deserializable hooks configuration.
///
/// Scripts are executed sequentially at session lifecycle boundaries.
/// Each entry is a path to a script file. The interpreter is determined
/// by file extension: `.sh` → bash, `.py` → python3, `.js` → node.
/// Files with no recognized extension are executed directly.
/// The `[git]` section: how an agent lands and proposes the work it did.
///
/// ```toml
/// [git]
/// auto_commit = true                    # commit after each turn that left changes
/// commit_agent = "claude-code"          # which agent writes the commits (default: the current one)
/// commit_rules = "Conventional Commits; one concern per commit; explain why"
/// pr_rules = "Open a PR per branch; title under 70 chars; wait for green CI before merging"
/// ```
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct GitConfigToml {
    /// Land the changes a turn leaves behind as commits, without being asked.
    #[serde(default)]
    pub auto_commit: Option<bool>,

    /// The agent that writes the commits. The current agent when unset.
    #[serde(default)]
    pub commit_agent: Option<String>,

    /// Conventions for commits: message style, how finely to split, what to leave out.
    #[serde(default)]
    pub commit_rules: Option<String>,

    /// What a pull request should look like, when work is ready to upstream, and how to check CI.
    #[serde(default)]
    pub pr_rules: Option<String>,
}

/// The resolved `[git]` section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitConfig {
    pub auto_commit: bool,
    pub commit_agent: Option<String>,
    pub commit_rules: Option<String>,
    pub pr_rules: Option<String>,
}

impl GitConfig {
    pub fn from_toml(toml: GitConfigToml) -> Self {
        let text = |value: Option<String>| {
            value
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        Self {
            auto_commit: toml.auto_commit.unwrap_or(false),
            commit_agent: text(toml.commit_agent),
            commit_rules: text(toml.commit_rules),
            pr_rules: text(toml.pr_rules),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct HooksConfigToml {
    /// Scripts to run when a session starts.
    #[serde(default)]
    pub session_start: Option<Vec<String>>,

    /// Scripts to run when a session ends.
    #[serde(default)]
    pub session_end: Option<Vec<String>>,

    /// Scripts to run before a user prompt is sent to the agent.
    #[serde(default)]
    pub pre_user_prompt: Option<Vec<String>>,

    /// Scripts to run after a user prompt is sent to the agent.
    #[serde(default)]
    pub post_user_prompt: Option<Vec<String>>,

    /// Scripts to run before a tool call is executed.
    #[serde(default)]
    pub pre_tool_call: Option<Vec<String>>,

    /// Scripts to run after a tool call completes.
    #[serde(default)]
    pub post_tool_call: Option<Vec<String>>,

    /// Scripts to run before the agent produces a response.
    #[serde(default)]
    pub pre_agent_response: Option<Vec<String>>,

    /// Scripts to run after the agent finishes its response.
    #[serde(default)]
    pub post_agent_response: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run when a session starts.
    #[serde(default)]
    pub async_session_start: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run when a session ends.
    #[serde(default)]
    pub async_session_end: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run before a user prompt is sent.
    #[serde(default)]
    pub async_pre_user_prompt: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run after a user prompt is sent.
    #[serde(default)]
    pub async_post_user_prompt: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run before a tool call is executed.
    #[serde(default)]
    pub async_pre_tool_call: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run after a tool call completes.
    #[serde(default)]
    pub async_post_tool_call: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run before the agent produces a response.
    #[serde(default)]
    pub async_pre_agent_response: Option<Vec<String>>,

    /// Async (fire-and-forget) scripts to run after the agent finishes its response.
    #[serde(default)]
    pub async_post_agent_response: Option<Vec<String>>,
}

/// Expand a leading `~` to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Resolve a list of hook path strings into `PathBuf`s with tilde expansion.
pub fn resolve_hook_paths(paths: Option<Vec<String>>) -> Vec<PathBuf> {
    paths
        .unwrap_or_default()
        .into_iter()
        .map(|s| expand_tilde(&s))
        .collect()
}

#[cfg(test)]
mod tests;
