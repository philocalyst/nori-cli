//! A session's select-style configuration options: the model, the thought level, the mode.

use crate::error::AgentError;
use crate::schema::Meta;
use crate::schema::SessionConfigOption;
use crate::schema::SessionConfigOptionCategory;
use crate::schema::SessionConfigSelectOption;

/// The id Nori reads the model from.
pub const MODEL: &str = "model";
/// The id Nori reads the reasoning effort from.
pub const THOUGHT_LEVEL: &str = "thought_level";
/// The id Nori reads the session mode from.
pub const MODE: &str = "mode";

/// One value a select option accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub value: String,
    pub label: String,
    pub description: Option<String>,
    /// Extra `_meta` on the choice, such as the effect of choosing it that Nori's clients show.
    pub meta: Option<Meta>,
}

impl Choice {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: None,
            meta: None,
        }
    }

    /// A line of help shown beside the choice.
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// The choice's `_meta`.
    pub fn with_meta(mut self, meta: Meta) -> Self {
        self.meta = Some(meta);
        self
    }
}

/// A select option and the value it currently has. The current value is always one of the
/// choices.
#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    id: String,
    name: String,
    description: Option<String>,
    category: SessionConfigOptionCategory,
    choices: Vec<Choice>,
    current: String,
}

impl Select {
    /// A select over `choices`. A `current` that is not among them is added as a choice of its
    /// own, so a model forced from configuration still shows as selected.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        category: SessionConfigOptionCategory,
        mut choices: Vec<Choice>,
        current: impl Into<String>,
    ) -> Self {
        let current = current.into();
        if !choices.iter().any(|choice| choice.value == current) {
            choices.push(Choice::new(current.clone(), current.clone()));
        }
        Self {
            id: id.into(),
            name: name.into(),
            description: None,
            category,
            choices,
            current,
        }
    }

    pub fn model(choices: Vec<Choice>, current: impl Into<String>) -> Self {
        Self::new(
            MODEL,
            "Model",
            SessionConfigOptionCategory::Model,
            choices,
            current,
        )
    }

    pub fn thought_level(choices: Vec<Choice>, current: impl Into<String>) -> Self {
        Self::new(
            THOUGHT_LEVEL,
            "Thought Level",
            SessionConfigOptionCategory::ThoughtLevel,
            choices,
            current,
        )
    }

    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn current(&self) -> &str {
        &self.current
    }

    /// Choose `value`, which must be one of the choices.
    pub fn set(&mut self, value: &str) -> Result<(), AgentError> {
        if !self.choices.iter().any(|choice| choice.value == value) {
            return Err(AgentError::invalid(format!(
                "{value:?} is not a value of the {} option",
                self.id
            )));
        }
        value.clone_into(&mut self.current);
        Ok(())
    }

    pub fn to_acp(&self) -> SessionConfigOption {
        let choices: Vec<SessionConfigSelectOption> = self
            .choices
            .iter()
            .map(|choice| {
                SessionConfigSelectOption::new(choice.value.clone(), choice.label.clone())
                    .description(choice.description.clone())
                    .meta(choice.meta.clone())
            })
            .collect();
        SessionConfigOption::select(
            self.id.clone(),
            self.name.clone(),
            self.current.clone(),
            choices,
        )
        .description(self.description.clone())
        .category(self.category.clone())
    }
}

/// All of a session's options.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Options {
    selects: Vec<Select>,
}

impl Options {
    pub fn new(selects: Vec<Select>) -> Self {
        Self { selects }
    }

    /// The current value of option `id`.
    pub fn current(&self, id: &str) -> Option<&str> {
        self.selects
            .iter()
            .find(|select| select.id == id)
            .map(Select::current)
    }

    /// Apply a `session/set_config_option` and return the options as they now stand.
    pub fn set(&mut self, id: &str, value: &str) -> Result<Vec<SessionConfigOption>, AgentError> {
        let select = self
            .selects
            .iter_mut()
            .find(|select| select.id == id)
            .ok_or_else(|| AgentError::invalid(format!("unknown configuration option {id:?}")))?;
        select.set(value)?;
        Ok(self.to_acp())
    }

    pub fn to_acp(&self) -> Vec<SessionConfigOption> {
        self.selects.iter().map(Select::to_acp).collect()
    }
}
