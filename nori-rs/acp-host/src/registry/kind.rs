//! The built-in agents, their providers and package managers.

use std::fmt;

use super::ModelInjection;

/// A model that an agent's ACP adapter does not advertise but that generally
/// works when forced via spawn-time model injection. Surfaced under the model
/// picker's "Other" section and deduplicated against the advertised catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtherModel {
    /// The model id forced through the agent's injection channel.
    pub id: &'static str,
    /// Friendly label shown in the picker.
    pub label: &'static str,
}

/// ACP agent identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    /// Anthropic's Claude Code CLI
    ClaudeCode,
    /// OpenAI's Codex CLI
    Codex,
    /// Google's Gemini CLI
    Gemini,
    /// OpenCode's multi-provider coding agent.
    OpenCode,
    /// Google's Antigravity agent.
    Antigravity,
}

impl AgentKind {
    /// Get the slug (string identifier) for this agent
    pub fn slug(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "claude-code",
            AgentKind::Codex => "codex",
            AgentKind::Gemini => "gemini",
            AgentKind::OpenCode => "opencode",
            AgentKind::Antigravity => "antigravity",
        }
    }

    /// Get the display name for this agent
    pub fn display_name(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Claude Code",
            AgentKind::Codex => "Codex",
            AgentKind::Gemini => "Gemini",
            AgentKind::OpenCode => "OpenCode",
            AgentKind::Antigravity => "Antigravity",
        }
    }

    /// Get the context window size (in tokens) for this agent.
    ///
    /// These are approximate values based on typical model configurations:
    /// - Claude Code: 1M tokens
    /// - Codex: 258K tokens
    /// - Gemini: 1M tokens
    /// - Antigravity: 1M tokens
    pub fn context_window_size(&self) -> i64 {
        match self {
            AgentKind::ClaudeCode => 1_000_000,
            AgentKind::Codex => 258_000,
            AgentKind::Gemini => 1_000_000,
            AgentKind::OpenCode => 200_000,
            AgentKind::Antigravity => 1_000_000,
        }
    }

    /// Get the provider for this agent
    pub fn provider(&self) -> Provider {
        match self {
            AgentKind::ClaudeCode => Provider::Anthropic,
            AgentKind::Codex => Provider::OpenAI,
            AgentKind::Gemini => Provider::Google,
            AgentKind::OpenCode => Provider::OpenCode,
            AgentKind::Antigravity => Provider::Google,
        }
    }

    /// Get the npm package name for the underlying agent (for installation detection)
    pub fn npm_package(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "@anthropic-ai/claude-code",
            AgentKind::Codex => "@openai/codex",
            AgentKind::Gemini => "@google/gemini-cli",
            AgentKind::OpenCode => "opencode-ai",
            AgentKind::Antigravity => "google-antigravity",
        }
    }

    /// Get the ACP adapter package name for launching this agent
    pub fn acp_package(&self) -> &'static str {
        match self {
            // @latest forces bunx to resolve the new scope instead of a stale
            // @zed-industries cache entry with the same unscoped package name.
            AgentKind::ClaudeCode => "@agentclientprotocol/claude-agent-acp@latest",
            AgentKind::Codex => "@agentclientprotocol/codex-acp@latest",
            // Gemini has native ACP support
            AgentKind::Gemini => "@google/gemini-cli",
            AgentKind::OpenCode => "opencode-ai",
            AgentKind::Antigravity => "google-antigravity",
        }
    }

    /// Get all agent variants
    pub fn all() -> &'static [AgentKind] {
        &[
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Gemini,
            AgentKind::OpenCode,
            AgentKind::Antigravity,
        ]
    }

    /// The out-of-band channel this built-in agent uses to receive a forced
    /// model id at spawn time. Verified against each adapter: Claude honors
    /// `ANTHROPIC_MODEL`, Gemini honors `GEMINI_MODEL` (both in ACP mode), and
    /// Codex reads the `model` key from its `CODEX_CONFIG` JSON.
    pub fn model_injection(&self) -> ModelInjection {
        match self {
            AgentKind::ClaudeCode => ModelInjection::Env {
                var: "ANTHROPIC_MODEL".to_string(),
            },
            AgentKind::Gemini => ModelInjection::Env {
                var: "GEMINI_MODEL".to_string(),
            },
            AgentKind::Codex => ModelInjection::CodexConfig,
            AgentKind::OpenCode => ModelInjection::None,
            // `agy acp` takes no model at start; the session's model option selects it.
            AgentKind::Antigravity => ModelInjection::None,
        }
    }

    /// Get the base directory for transcript files, relative to home directory.
    ///
    /// Returns the path where this agent stores session transcript files.
    pub fn transcript_base_dir(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => ".claude/projects",
            AgentKind::Codex => ".codex/sessions",
            AgentKind::Gemini => ".gemini/tmp",
            AgentKind::OpenCode => ".local/share/opencode",
            AgentKind::Antigravity => ".gemini/antigravity",
        }
    }

    /// Get authentication hint for this agent.
    ///
    /// Returns actionable instructions on how to authenticate with this agent's provider.
    pub fn auth_hint(&self) -> &'static str {
        match self {
            AgentKind::ClaudeCode => "Run /login for instructions, or set ANTHROPIC_API_KEY.",
            AgentKind::Codex => "Run /login to authenticate, or set OPENAI_API_KEY.",
            AgentKind::Gemini => "Run /login for instructions, or set GOOGLE_API_KEY.",
            AgentKind::OpenCode => "Run /login or `opencode auth login` to configure a provider.",
            AgentKind::Antigravity => {
                "Run `agy acp --login` to sign in with Google. Using an API key instead? Select the \
                 `antigravity-sdk` agent (see docs/nix.md) and set GEMINI_API_KEY."
            }
        }
    }

    /// Parse an agent from a string slug
    pub fn from_slug(slug: &str) -> Option<AgentKind> {
        let normalized = slug.to_lowercase();
        match normalized.as_str() {
            "claude-code" | "claude" | "claude-acp" | "claude-4.5" => Some(AgentKind::ClaudeCode),
            "codex" | "codex-acp" => Some(AgentKind::Codex),
            "gemini" | "gemini-acp" | "gemini-2.5-flash" => Some(AgentKind::Gemini),
            "opencode" | "opencode-acp" => Some(AgentKind::OpenCode),
            "antigravity" | "agy" | "antigravity-acp" => Some(AgentKind::Antigravity),
            _ => None,
        }
    }

    /// Curated models this agent's ACP adapter does not advertise but that
    /// generally work when forced via spawn-time injection. Shown under the
    /// model picker's "Other" section; deduplicated at render time against the
    /// agent's advertised catalog.
    pub fn other_models(self) -> &'static [OtherModel] {
        match self {
            AgentKind::ClaudeCode => &[
                OtherModel {
                    id: "claude-opus-4-8",
                    label: "Opus 4.8",
                },
                OtherModel {
                    id: "claude-opus-4-7",
                    label: "Opus 4.7",
                },
                OtherModel {
                    id: "claude-opus-4-6",
                    label: "Opus 4.6",
                },
                OtherModel {
                    id: "claude-sonnet-4-6",
                    label: "Sonnet 4.6",
                },
            ],
            AgentKind::Codex => &[OtherModel {
                id: "gpt-5.3-codex",
                label: "GPT-5.3-Codex",
            }],
            AgentKind::Gemini => &[
                OtherModel {
                    id: "gemini-3.7-flash",
                    label: "Gemini 3.7 Flash",
                },
                OtherModel {
                    id: "gemini-3.5-flash",
                    label: "Gemini 3.5 Flash",
                },
                OtherModel {
                    id: "gemini-3.5-flash-lite",
                    label: "Gemini 3.5 Flash-Lite",
                },
            ],
            AgentKind::OpenCode => &[],
            AgentKind::Antigravity => &[],
        }
    }
}

impl fmt::Display for AgentKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.slug())
    }
}

/// Model provider identifier
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    /// Anthropic
    Anthropic,
    /// OpenAI
    OpenAI,
    /// Google
    Google,
    /// OpenCode can connect to several providers.
    OpenCode,
}

impl Provider {
    /// Get the slug (string identifier) for this provider
    pub fn slug(&self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
            Provider::OpenAI => "openai",
            Provider::Google => "google",
            Provider::OpenCode => "opencode",
        }
    }

    /// Get the display name for this provider
    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::Anthropic => "Anthropic",
            Provider::OpenAI => "OpenAI",
            Provider::Google => "Google",
            Provider::OpenCode => "OpenCode",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.slug())
    }
}

/// Package manager used to install/run the agent
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PackageManager {
    /// npm (Node Package Manager)
    Npm,
    /// bun
    Bun,
}

impl PackageManager {
    /// Get the command name for this package manager
    pub fn command(&self) -> &'static str {
        match self {
            PackageManager::Npm => "npx",
            PackageManager::Bun => "bunx",
        }
    }

    /// Get the display name for this package manager
    pub fn display_name(&self) -> &'static str {
        match self {
            PackageManager::Npm => "npm",
            PackageManager::Bun => "bun",
        }
    }
}

impl fmt::Display for PackageManager {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}
