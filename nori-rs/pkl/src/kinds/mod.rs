//! One typed Rust binding per declarative kind.
//!
//! Each type is what a module of that kind evaluates to: serde types with
//! `deny_unknown_fields`, so a module that does not fit its type is an error that names the
//! field. [`JobDef`], [`MonitorDef`], [`PluginDef`] and [`GateDef`] match the `nori:Job`,
//! `nori:Monitor`, `nori:Plugin` and `nori:Gate` schemas exactly and expose `validate` for everything a type cannot say.
//! [`SkillMeta`] is the part of a skill the catalog needs; the program belongs to `nori-conduit`.
//!
//! Durations are `std::time::Duration` here; the evaluator hands them over as `{value, unit}`.

pub mod gate;
pub mod job;
pub mod monitor;
pub mod plugin;
pub mod skill;

pub use gate::GateDef;
pub use gate::GateEvent;
pub use job::JobDef;
pub use job::JobWake;
pub use job::Ready;
pub use job::Restart;
pub use job::RestartOn;
pub use monitor::Action;
pub use monitor::ActorTrack;
pub use monitor::JobEvent;
pub use monitor::MonitorDef;
pub use monitor::SessionEvent;
pub use monitor::Trigger;
pub use monitor::WakeMode;
pub use monitor::WatchKind;
pub use plugin::ChoiceFrom;
pub use plugin::Command;
pub use plugin::CommandCategory;
pub use plugin::CommandEffect;
pub use plugin::CommandRequires;
pub use plugin::Operation;
pub use plugin::Parameter;
pub use plugin::PluginDef;
pub use skill::SkillMeta;
pub use skill::SkillParameter;

use crate::recovery::DurationValue;

/// Serde adapter: a Pkl `Duration` as a [`std::time::Duration`].
pub(crate) mod duration {
    use std::time::Duration;

    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::de::Error;

    use crate::recovery::DurationValue;

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        DurationValue::deserialize(deserializer)?
            .to_duration()
            .map_err(D::Error::custom)
    }

    pub fn serialize<S: Serializer>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        super::seconds(*duration).serialize(serializer)
    }
}

/// Serde adapter: an optional Pkl `Duration`.
pub(crate) mod optional_duration {
    use std::time::Duration;

    use serde::Deserialize;
    use serde::Deserializer;
    use serde::Serialize;
    use serde::Serializer;
    use serde::de::Error;

    use crate::recovery::DurationValue;

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Option::<DurationValue>::deserialize(deserializer)?
            .map(|value| value.to_duration().map_err(D::Error::custom))
            .transpose()
    }

    pub fn serialize<S: Serializer>(
        duration: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        duration.map(super::seconds).serialize(serializer)
    }
}

fn seconds(duration: std::time::Duration) -> DurationValue {
    DurationValue {
        value: duration.as_secs_f64(),
        unit: "s".to_owned(),
    }
}

/// Records a problem unless `name` is a module name: `[a-z0-9][a-z0-9_-]*`.
pub(crate) fn check_name(field: &str, name: &str, problems: &mut Vec<String>) {
    let mut chars = name.chars();
    let starts_well = chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit());
    let rest_is_well =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !starts_well || !rest_is_well {
        problems.push(format!(
            "{field}: {name:?} is not a valid name (lowercase letters, digits, `-` and `_`, starting with a letter or digit)"
        ));
    }
}

/// Records a problem unless `pattern` compiles as a regular expression.
pub(crate) fn check_regex(field: &str, pattern: &str, problems: &mut Vec<String>) {
    if let Err(error) = regex::Regex::new(pattern) {
        let message = error.to_string();
        let detail = message.lines().next_back().unwrap_or(&message);
        problems.push(format!(
            "{field}: {pattern:?} is not a valid regular expression ({})",
            detail.trim_start_matches("error: ")
        ));
    }
}

/// Records a problem when `text` is empty or only whitespace.
pub(crate) fn check_not_empty(field: &str, text: &str, problems: &mut Vec<String>) {
    if text.trim().is_empty() {
        problems.push(format!("{field}: must not be empty"));
    }
}

/// `Ok` when no problem was recorded.
pub(crate) fn finish(problems: Vec<String>) -> Result<(), Vec<String>> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}
