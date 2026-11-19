//! The models and reasoning efforts a session offers.

use antigravity_sdk_rust::types::ThinkingLevel;
use nori_acp_kit::Choice;
use nori_acp_kit::MODEL;
use nori_acp_kit::Options;
use nori_acp_kit::Select;
use nori_acp_kit::THOUGHT_LEVEL;

pub(crate) const DEFAULT_MODEL: &str = "gemini-4-carbon";
pub(crate) const DEFAULT_THOUGHT_LEVEL: &str = "medium";

/// `(id, label)`.
const MODELS: &[(&str, &str)] = &[
    ("gemini-4-carbon", "Gemini 4 Carbon"),
    ("gemini-4-hydrogen", "Gemini 4 Hydrogen"),
    ("gemini-4-argon", "Gemini 4 Argon"),
];

/// `(id, label, sdk level)`.
const THOUGHT_LEVELS: &[(&str, &str, ThinkingLevel)] = &[
    ("low", "Low", ThinkingLevel::Low),
    ("medium", "Medium", ThinkingLevel::Medium),
    ("high", "High", ThinkingLevel::High),
    ("max", "Max", ThinkingLevel::ExtraHigh),
];

pub(crate) fn is_thought_level(value: &str) -> bool {
    THOUGHT_LEVELS.iter().any(|(id, _, _)| *id == value)
}

/// A session's options. A model outside the curated list (one forced from configuration)
/// stays selectable; a thought level that is not one of ours falls back to the default, since
/// the caller validated what the user typed and this only restores stored sessions.
pub(crate) fn options(model: Option<&str>, thought_level: Option<&str>) -> Options {
    let model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_MODEL);
    let level = thought_level
        .map(str::trim)
        .filter(|value| is_thought_level(value))
        .unwrap_or(DEFAULT_THOUGHT_LEVEL);
    Options::new(vec![
        Select::model(
            MODELS
                .iter()
                .map(|(id, label)| Choice::new(*id, *label))
                .collect(),
            model,
        )
        .describe("Antigravity model"),
        Select::thought_level(
            THOUGHT_LEVELS
                .iter()
                .map(|(id, label, _)| Choice::new(*id, *label))
                .collect(),
            level,
        )
        .describe("Reasoning depth for the active model"),
    ])
}

pub(crate) fn model(options: &Options) -> String {
    options.current(MODEL).unwrap_or(DEFAULT_MODEL).to_owned()
}

pub(crate) fn thought_level(options: &Options) -> String {
    options
        .current(THOUGHT_LEVEL)
        .unwrap_or(DEFAULT_THOUGHT_LEVEL)
        .to_owned()
}

pub(crate) fn thinking_level(options: &Options) -> ThinkingLevel {
    let current = thought_level(options);
    THOUGHT_LEVELS
        .iter()
        .find(|(id, _, _)| *id == current)
        .map_or(ThinkingLevel::Medium, |(_, _, level)| *level)
}
