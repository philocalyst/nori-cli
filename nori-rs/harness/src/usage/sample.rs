//! One line of `usage/YYYY-MM.jsonl`.

use chrono::DateTime;
use chrono::Utc;
use nori_protocol::RateLimitWindow;
use nori_protocol::UsageTokens;
use serde::Deserialize;
use serde::Serialize;

/// Tokens one session spent (and the quota state it saw) at one moment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageSample {
    /// RFC 3339, UTC.
    pub ts: String,
    pub session_id: String,
    /// The session this one was delegated from, when it is a subagent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_session_id: Option<String>,
    /// The delegated actor that spent it (its id as the user's view shows it), so a program
    /// can charge a turn to the actor that ran it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// The agent that ran it ("claude-code", "codex").
    pub agent: String,
    /// Whose tokens these are ("claude", "codex", "gemini"): the grouping the view shows.
    pub provider: String,
    pub model: String,
    /// Spent since the previous sample of this session.
    #[serde(default)]
    pub tokens: UsageTokens,
    /// What the provider charged for these tokens, when it said. Otherwise the price table
    /// prices them when the summary is read, so a changed override applies to old samples too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    /// Which account the quota belongs to, so sessions on one account pool their windows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rate_limits: Vec<RateLimitWindow>,
}

impl UsageSample {
    pub fn time(&self) -> Option<DateTime<Utc>> {
        DateTime::parse_from_rfc3339(&self.ts)
            .ok()
            .map(|time| time.with_timezone(&Utc))
    }
}

/// A provider's name as the user reads it.
pub fn provider_title(provider: &str) -> String {
    match provider {
        "opencode-go" => "OpenCode Go".to_owned(),
        "opencode" => "OpenCode Models".to_owned(),
        "claude" => "Claude".to_owned(),
        "codex" => "Codex".to_owned(),
        "gemini" => "Gemini".to_owned(),
        "deepseek" => "DeepSeek".to_owned(),
        "grok" => "Grok".to_owned(),
        "antigravity" => "Antigravity".to_owned(),
        other => {
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// The provider a model's tokens are billed by, from the model id and, failing that, the agent.
pub fn provider_for(agent: &str, model: &str) -> String {
    const FAMILIES: [(&str, &[&str]); 6] = [
        (
            "claude",
            &["claude", "anthropic", "haiku", "sonnet", "opus"],
        ),
        ("codex", &["gpt", "codex", "openai", "o1", "o3", "o4"]),
        ("gemini", &["gemini", "gemma"]),
        ("deepseek", &["deepseek"]),
        ("grok", &["grok"]),
        ("antigravity", &["antigravity"]),
    ];
    let model = model.to_ascii_lowercase();
    let agent = agent.to_ascii_lowercase();
    if model.starts_with("opencode-go/") {
        return "opencode-go".to_owned();
    }
    if model.starts_with("opencode/") {
        return "opencode".to_owned();
    }
    for subject in [&model, &agent] {
        for (provider, needles) in FAMILIES {
            let hit = needles.iter().any(|needle| {
                if needle.len() <= 2 {
                    subject.starts_with(needle)
                } else {
                    subject.contains(needle)
                }
            });
            if hit {
                return provider.to_owned();
            }
        }
    }
    if agent.is_empty() {
        "other".to_owned()
    } else {
        agent
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::provider_for;
    use super::provider_title;

    #[test]
    fn opencode_go_and_models_keep_separate_provider_attribution() {
        assert_eq!(
            provider_for("opencode", "opencode-go/deepseek-v4.1-flash"),
            "opencode-go"
        );
        assert_eq!(
            provider_for("opencode", "opencode/deepseek-v4.1-flash"),
            "opencode"
        );
        assert_eq!(provider_title("opencode-go"), "OpenCode Go");
        assert_eq!(provider_title("opencode"), "OpenCode Models");
    }
}
