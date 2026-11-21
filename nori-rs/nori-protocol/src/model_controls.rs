//! The dials a model has besides its name: how hard it thinks (effort) and how it is served
//! (the model mode).
//!
//! Agents expose these as ACP session config options, each in its own words: Claude Code's
//! `fast` toggle with `on` and `off`, Codex's `service_tier` with `default`, `priority` and
//! `flex`, a thought level with `low` .. `max`. Everything that reads or steps them goes through
//! this module, so no client matches on an agent's option ids or value strings of its own:
//!
//! - [`ModelMode`] is the typed service mode. An agent's value is mapped into it in one place,
//!   [`ModelMode::from_choice`]; a value Nori does not know stays visible as
//!   [`ModelMode::Other`] with the agent's own label.
//! - [`Dial`] names the three things a user steps through: the model, the effort and the mode.
//! - [`ladder`] is the order a dial's values are listed and stepped in, [`step`] moves along it,
//!   and says when it wrapped around.

use serde::Deserialize;
use serde::Serialize;

use crate::acp::v1 as acp;

/// How a model is served: the speed and price tier of a request.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelMode {
    /// The agent's ordinary service.
    Standard,
    /// A faster mode: Claude Code's fast mode.
    Fast,
    /// OpenAI's `flex` tier: cheaper, and slower under load.
    Flex,
    /// OpenAI's `priority` tier: faster, at a higher price.
    Priority,
    /// A mode this version of Nori does not know, under the name the agent gives it.
    Other(String),
}

impl ModelMode {
    /// The mode `name` spells, if it is one Nori knows. Matching ignores case, `_`, `-` and
    /// spaces. `on` and `true` are how an agent says a fast-mode toggle is set, so they mean
    /// [`ModelMode::Fast`] and are only read this way for an option [`is_mode_option`] accepts.
    pub fn named(name: &str) -> Option<Self> {
        match squash(name).as_str() {
            "standard" | "default" | "normal" | "regular" | "off" | "none" | "false" => {
                Some(Self::Standard)
            }
            "fast" | "on" | "true" => Some(Self::Fast),
            "flex" => Some(Self::Flex),
            "priority" => Some(Self::Priority),
            _ => None,
        }
    }

    /// What the agent's choice `value`, shown as `label`, means: the value is read first, then
    /// the label, and a choice that is neither is kept as the agent names it.
    pub fn from_choice(value: &str, label: &str) -> Self {
        Self::named(value)
            .or_else(|| Self::named(label))
            .unwrap_or_else(|| Self::Other(if label.is_empty() { value } else { label }.to_owned()))
    }

    /// How the mode is written to the user.
    pub fn label(&self) -> &str {
        match self {
            Self::Standard => "Standard",
            Self::Fast => "Fast",
            Self::Flex => "Flex",
            Self::Priority => "Priority",
            Self::Other(label) => label,
        }
    }

    pub fn is_standard(&self) -> bool {
        *self == Self::Standard
    }

    /// Where the mode sits from slowest to fastest: flex, standard, fast, priority. A mode Nori
    /// does not know goes last.
    fn rank(&self) -> i32 {
        match self {
            Self::Flex => 0,
            Self::Standard => 1,
            Self::Fast => 2,
            Self::Priority => 3,
            Self::Other(_) => 4,
        }
    }
}

/// What a user steps through.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dial {
    #[default]
    Model,
    /// The reasoning effort or thought level.
    Effort,
    /// The model mode: standard, fast, flex, priority.
    Mode,
}

impl Dial {
    /// Whether `option` is the agent's selector for this dial.
    pub fn selects(self, option: &acp::SessionConfigOption) -> bool {
        match self {
            Self::Model => option.category == Some(acp::SessionConfigOptionCategory::Model),
            Self::Effort => is_effort_option(option),
            Self::Mode => is_mode_option(option),
        }
    }

    /// What the dial is called in a sentence.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Effort => "effort level",
            Self::Mode => "model mode",
        }
    }
}

/// Whether `option` is the agent's reasoning effort: the option it declares as its thought
/// level, or, for an agent that declares no category for it, the one named for effort or
/// reasoning.
pub fn is_effort_option(option: &acp::SessionConfigOption) -> bool {
    match &option.category {
        Some(category) => *category == acp::SessionConfigOptionCategory::ThoughtLevel,
        None => {
            let id = option.id.to_string().to_lowercase();
            id.contains("effort") || id.contains("thought") || id.contains("reasoning")
        }
    }
}

/// The ids agents give their service-mode option.
const MODE_OPTION_IDS: [&str; 8] = [
    "fast",
    "fastmode",
    "servicetier",
    "tier",
    "speed",
    "speedmode",
    "modelmode",
    "processingtier",
];

/// Whether `option` is the agent's selector for how the model is served: a select that is not
/// the model, the permission mode or the effort, and that is named for a tier or speed or
/// offers a `fast`, `flex` or `priority` value.
pub fn is_mode_option(option: &acp::SessionConfigOption) -> bool {
    let acp::SessionConfigKind::Select(select) = &option.kind else {
        return false;
    };
    if matches!(
        option.category,
        Some(
            acp::SessionConfigOptionCategory::Model
                | acp::SessionConfigOptionCategory::Mode
                | acp::SessionConfigOptionCategory::ThoughtLevel
        )
    ) || is_effort_option(option)
    {
        return false;
    }
    if MODE_OPTION_IDS.contains(&squash(&option.id.to_string()).as_str()) {
        return true;
    }
    select_values(&select.options).iter().any(|value| {
        matches!(
            squash(&value.value.to_string()).as_str(),
            "fast" | "flex" | "priority"
        )
    })
}

/// The current model mode of `option`, when it is a mode option.
pub fn mode_of(option: &acp::SessionConfigOption) -> Option<ModelMode> {
    if !is_mode_option(option) {
        return None;
    }
    let acp::SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let current = select.current_value.to_string();
    let label = select_values(&select.options)
        .into_iter()
        .find(|value| value.value.to_string() == current)
        .map_or(current.clone(), |value| value.name.clone());
    Some(ModelMode::from_choice(&current, &label))
}

/// The model mode the agent's options say the session runs in. None when the agent offers no
/// mode, which is the same as the standard one for what a user sees.
pub fn current_mode(options: &[acp::SessionConfigOption]) -> Option<ModelMode> {
    options.iter().find_map(mode_of)
}

/// One value of a select option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rung {
    pub value: String,
    pub label: String,
}

/// The values `option` offers, in the order they are listed and stepped through for `dial`.
///
/// Models keep the agent's order, which is the order `/model` lists. Effort runs from least to
/// most: a `default` entry first, then none, minimal, low, medium, high, xhigh, max, ultra,
/// persistent as far as the agent offers them, then anything else in the agent's order. Modes
/// run from slowest to fastest: flex, standard, fast, priority, then anything else.
pub fn ladder(dial: Dial, option: &acp::SessionConfigOption) -> Vec<Rung> {
    let acp::SessionConfigKind::Select(select) = &option.kind else {
        return Vec::new();
    };
    let mut rungs: Vec<Rung> = select_values(&select.options)
        .into_iter()
        .map(|value| Rung {
            value: value.value.to_string(),
            label: value.name.clone(),
        })
        .collect();
    // The sort is stable: values with the same standing stay in the agent's order.
    rungs.sort_by_key(|rung| standing(dial, &rung.value, &rung.label));
    rungs
}

/// Where a value of `dial`'s option stands in the order it is listed and stepped in. A client that
/// lists the values itself (a picker, which has more to show than a [`Rung`]) sorts by this.
pub fn standing(dial: Dial, value: &str, label: &str) -> (i32, i32) {
    match dial {
        Dial::Model => (0, 0),
        Dial::Effort => effort_standing(value, label),
        Dial::Mode => (ModelMode::from_choice(value, label).rank(), 0),
    }
}

/// Where an effort value stands: the unset `default` first, then the known levels by strength,
/// then any other.
fn effort_standing(value: &str, label: &str) -> (i32, i32) {
    let level = |name: &str| match squash(name).as_str() {
        "none" => Some(0),
        "minimal" => Some(1),
        "low" => Some(2),
        "medium" | "med" => Some(3),
        "high" => Some(4),
        "xhigh" | "extrahigh" => Some(5),
        "max" | "maximum" => Some(6),
        "ultra" => Some(7),
        "persistent" => Some(8),
        _ => None,
    };
    if matches!(squash(value).as_str(), "default" | "auto" | "recommended") {
        return (0, 0);
    }
    level(value)
        .or_else(|| level(label))
        .map_or((2, 0), |level| (1, level))
}

/// Which way a press moves along a ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

/// Where a step landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stepped {
    pub to: Rung,
    /// The step ran past one end of the ladder and came out at the other. Stepping never wraps
    /// by accident: whoever shows the result says so.
    pub wrapped: bool,
}

/// One step from `current` along `ladder`. A `current` the ladder does not list (a model typed in
/// by hand) is not on it, so a step forward lands on its first value and a step back on its last.
/// None when there is nothing to choose between.
pub fn step(ladder: &[Rung], current: &str, direction: Direction) -> Option<Stepped> {
    let last = ladder.len().checked_sub(1).filter(|last| *last > 0)?;
    let position = ladder.iter().position(|rung| rung.value == current);
    let (index, wrapped) = match (position, direction) {
        (None, Direction::Forward) => (0, false),
        (None, Direction::Backward) => (last, false),
        (Some(at), Direction::Forward) if at == last => (0, true),
        (Some(at), Direction::Forward) => (at + 1, false),
        (Some(0), Direction::Backward) => (last, true),
        (Some(at), Direction::Backward) => (at - 1, false),
    };
    Some(Stepped {
        to: ladder[index].clone(),
        wrapped,
    })
}

/// The setting `setting` names among `options`. Two names are typed and mean the same thing for
/// every agent: `model_mode` is the agent's service-mode option and `effort` its reasoning
/// effort. Any other name is an option's id or its name.
pub fn find_setting<'a>(
    options: &'a [acp::SessionConfigOption],
    setting: &str,
) -> Option<&'a acp::SessionConfigOption> {
    let wanted = squash(setting);
    match wanted.as_str() {
        "modelmode" => options.iter().find(|option| is_mode_option(option)),
        "effort" => options.iter().find(|option| is_effort_option(option)),
        _ => options.iter().find(|option| {
            squash(&option.id.to_string()) == wanted || squash(&option.name) == wanted
        }),
    }
}

/// The value of `option` that `wanted` asks for: one of its values or labels, ignoring case, or,
/// for a mode option, the mode that name spells (`fast` finds the `on` of a fast toggle).
pub fn value_for(option: &acp::SessionConfigOption, wanted: &str) -> Option<String> {
    let rungs = ladder(Dial::Model, option);
    let spelled = rungs.iter().find(|rung| rung.value == wanted).or_else(|| {
        rungs.iter().find(|rung| {
            rung.value.eq_ignore_ascii_case(wanted) || rung.label.eq_ignore_ascii_case(wanted)
        })
    });
    if let Some(rung) = spelled {
        return Some(rung.value.clone());
    }
    if !is_mode_option(option) {
        return None;
    }
    let mode = ModelMode::named(wanted)?;
    rungs
        .into_iter()
        .find(|rung| ModelMode::from_choice(&rung.value, &rung.label) == mode)
        .map(|rung| rung.value)
}

fn select_values(
    options: &acp::SessionConfigSelectOptions,
) -> Vec<&acp::SessionConfigSelectOption> {
    match options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        // The schema's option layouts are not exhaustive; an unknown one offers nothing that
        // can be listed.
        _ => Vec::new(),
    }
}

fn squash(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' '))
        .flat_map(char::to_lowercase)
        .collect()
}
