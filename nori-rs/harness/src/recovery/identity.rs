//! What model a route runs, independent of which agent serves it.
//!
//! Agents name the same model differently (`openai/gpt-6.1-terra`, `gpt-6.1-terra`, a display
//! name such as "GPT 6.1 Terra"). [`ModelIdentity`] reduces those to one comparable key, so that
//! pickers can show one row per model and recovery can find the same model on another agent.

/// The vendor family of a model, which decides which agents are native for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Claude,
    Gpt,
    Gemini,
}

impl ModelFamily {
    /// Agents that talk to the model's vendor directly, best first. Every other agent that
    /// offers the model reaches it through a gateway.
    pub fn native_agents(self) -> &'static [&'static str] {
        match self {
            ModelFamily::Claude => &["claude-code"],
            ModelFamily::Gpt => &["codex"],
            ModelFamily::Gemini => &["gemini", "antigravity"],
        }
    }

    /// Whether `agent` talks to the model's vendor directly. A second profile of a native agent
    /// (`claude2` beside `claude-code`'s `claude-code`) is native too.
    pub fn is_native(self, agent: &str) -> bool {
        let stem = agent
            .split_once('@')
            .map_or(agent, |(provider, _)| provider)
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .trim_end_matches(['-', '_']);
        self.native_agents().iter().any(|native| {
            *native == agent
                || *native == stem
                || native
                    .split('-')
                    .next()
                    .is_some_and(|vendor| !vendor.is_empty() && vendor == stem)
        })
    }

    fn of_key(key: &str) -> Option<Self> {
        let starts = |names: &[&str]| names.iter().any(|name| key.starts_with(name));
        if starts(&["gpt", "astra", "terra", "sol", "luna"]) {
            Some(ModelFamily::Gpt)
        } else if starts(&["claude", "fable", "opus", "sonnet", "haiku"]) {
            Some(ModelFamily::Claude)
        } else if starts(&["gemini"]) {
            Some(ModelFamily::Gemini)
        } else {
            None
        }
    }
}

/// A model as its routes agree on it: a normalised name, a numeric version and a named variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelIdentity {
    pub key: String,
    pub version: Option<String>,
    pub variant: Option<String>,
}

impl ModelIdentity {
    /// The identity of a model an agent calls `value` and shows as `name`. A versioned display
    /// name identifies a model even when the agent advertises a generic value such as `sonnet`.
    pub fn of(name: &str, value: &str) -> Self {
        let label = model_label(value, name, None);
        let label = label.as_str();
        let selected = if label.chars().any(|ch| ch.is_ascii_digit()) {
            label
        } else {
            value.rsplit_once('/').map_or(value, |(_, suffix)| suffix)
        };
        let key = normalized(selected);
        Self {
            key: if matches!(key.as_str(), "default" | "auto" | "automatic") {
                "automatic".into()
            } else {
                key
            },
            version: version(value),
            variant: variant(value),
        }
    }

    /// Whether two routes serve the same model: identical names, and where both state a numeric
    /// version, identical versions and variants. Floating names (`sonnet`) match any version.
    pub fn is_same_model(&self, other: &Self) -> bool {
        self.key == other.key
            && (self.version.is_none()
                || other.version.is_none()
                || (self.version == other.version && self.variant == other.variant))
    }

    pub fn family(&self) -> Option<ModelFamily> {
        ModelFamily::of_key(&self.key)
    }
}

/// A model's label without its namespace or its provider: `OpenAI/GPT 6 Terra` and
/// `GPT 6 Terra (OpenAI)` are both `GPT 6 Terra` for provider `OpenAI`. Nothing else is removed.
pub fn display_name<'a>(name: &'a str, provider: Option<&str>) -> &'a str {
    let name = name.rsplit_once('/').map_or(name, |(_, name)| name).trim();
    provider
        .and_then(|provider| name.strip_suffix(&format!(" ({provider})")))
        .map_or(name, str::trim)
}

/// Model families whose ids start with the family's name, and how the family is written.
const FAMILIES: &[(&str, &str)] = &[
    ("gpt", "GPT"),
    ("claude", "Claude"),
    ("gemini", "Gemini"),
    ("deepseek", "DeepSeek"),
    ("grok", "Grok"),
    ("llama", "Llama"),
    ("qwen", "Qwen"),
    ("kimi", "Kimi"),
    ("glm", "GLM"),
    ("mistral", "Mistral"),
];

/// The id of a model without its namespace and its `[parameters]`, lowercase.
fn bare_id(value: &str) -> String {
    let value = value.split('[').next().unwrap_or(value);
    value
        .rsplit_once('/')
        .map_or(value, |(_, id)| id)
        .trim()
        .to_lowercase()
}

/// An id such as `claude-opus-5-5` as a name: words capitalised, the digits of a version joined
/// by dots, the family written the way its vendor writes it (`Claude Opus 5.5`, `GPT-6 Luna`).
fn prettify_id(bare: &str) -> String {
    let mut pieces: Vec<String> = Vec::new();
    let mut previous_was_version = false;
    for token in bare
        .split(['-', '_', ' '])
        .filter(|token| !token.is_empty())
    {
        let is_version = token.chars().all(|ch| ch.is_ascii_digit() || ch == '.');
        if is_version && previous_was_version {
            if let Some(last) = pieces.last_mut() {
                last.push('.');
                last.push_str(token);
            }
        } else if let Some((_, label)) = FAMILIES.iter().find(|(key, _)| *key == token) {
            pieces.push((*label).to_owned());
        } else {
            let mut chars = token.chars();
            pieces.push(match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            });
        }
        previous_was_version = is_version;
    }
    // GPT is hyphenated to its version: `GPT-6 Luna`.
    if pieces.first().is_some_and(|first| first == "GPT")
        && pieces
            .get(1)
            .is_some_and(|next| next.starts_with(|ch: char| ch.is_ascii_digit()))
    {
        let version = pieces.remove(1);
        pieces[0] = format!("GPT-{version}");
    }
    pieces.join(" ")
}

/// How a model is written everywhere Nori shows it: the model picker, the status line, lanes and
/// route-change notes. `value` is the id the agent uses, `name` what it calls the model.
///
/// - An agent that lists the bare id as the name (`claude-opus-5-5`) is shown the id made
///   readable: `Claude Opus 5.5`.
/// - A name that leaves out the family its id carries (`6 Luna` for `gpt-6-luna`) gets the family
///   back: `GPT-6 Luna`.
/// - Otherwise it is the agent's own name, without a gateway namespace. A label is never
///   invented that the id does not support.
pub fn model_label(value: &str, name: &str, provider: Option<&str>) -> String {
    let base = display_name(name, provider);
    let bare = bare_id(value);
    let is_id = !base.is_empty()
        && base
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        && normalized(base) == normalized(&bare);
    if is_id && base.chars().any(|ch| ch.is_ascii_alphabetic()) && base.contains(['-', '_', '.']) {
        return prettify_id(&bare);
    }
    let family = bare
        .split(|ch: char| matches!(ch, '-' | '_' | '.') || ch.is_ascii_digit())
        .next()
        .and_then(|first| FAMILIES.iter().find(|(key, _)| *key == first));
    match family {
        Some((key, label)) if !base.to_lowercase().contains(key) => {
            if *label == "GPT" && base.starts_with(|ch: char| ch.is_ascii_digit()) {
                format!("GPT-{base}")
            } else {
                format!("{label} {base}")
            }
        }
        Some(_) | None => base.to_owned(),
    }
}

/// Lowercase alphanumerics, keeping the dot of a version number (`6.1`, from `6-1` or `6_1`).
pub fn normalized(value: &str) -> String {
    let chars: Vec<char> = value.to_lowercase().chars().collect();
    let mut key = String::new();
    for (index, ch) in chars.iter().copied().enumerate() {
        if ch.is_alphanumeric() {
            key.push(ch);
        } else if matches!(ch, '.' | '-' | '_')
            && index > 0
            && index + 1 < chars.len()
            && chars[index - 1].is_ascii_digit()
            && chars[index + 1].is_ascii_digit()
        {
            key.push('.');
        }
    }
    key
}

/// The numbers in the model id after its provider prefix, joined with dots (`gpt-6-1` is `6.1`).
pub fn version(value: &str) -> Option<String> {
    // `[context=300k,fast=true]` parameterises a model; it is not part of its version. A run of
    // many digits is a release date (`20251001`), not a version.
    let value = value.split('[').next().unwrap_or(value);
    let value = value.rsplit_once('/').map_or(value, |(_, suffix)| suffix);
    let numbers = value
        .split(|ch: char| !ch.is_ascii_digit())
        .filter(|part| !part.is_empty() && part.len() <= MAX_VERSION_DIGITS)
        .collect::<Vec<_>>();
    (!numbers.is_empty()).then(|| numbers.join("."))
}

/// The most digits one component of a version has; more is a date.
const MAX_VERSION_DIGITS: usize = 4;

/// The variant of a model named in its id (`fast`, `thinking`, ...), if any.
pub fn variant(value: &str) -> Option<String> {
    let value = value.split('[').next().unwrap_or(value);
    let suffix = value.rsplit_once('/').map_or(value, |(_, suffix)| suffix);
    version(suffix)?;
    let tokens = suffix
        .to_ascii_lowercase()
        .split(['-', '_', '.'])
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ["fast", "thinking", "mini", "lite", "preview"]
        .into_iter()
        .find(|variant| tokens.iter().any(|token| token == variant))
        .map(str::to_owned)
}
