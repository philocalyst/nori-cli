//! A session's dials (model, effort, mode, and the flavor's own, such as Codex's service tier) as
//! ACP select options.
//!
//! The effort option only ever offers what the selected model accepts: switching to a model that
//! does not take the current effort moves the effort to the nearest one it does take, instead of
//! failing the next request. A flavor dial likewise falls back to its default when the new model
//! does not offer the current choice.
//!
//! Each choice of the model, effort and mode options declares what choosing it does (`_meta.nori
//! .dial`, a [`DialEffect`]): a change is never refused, and the client shows its consequence where
//! it is made. Model and effort changes take hold with the turn's next request, a mode at once; a
//! different model also drops the prompt cache and may shrink the context window.

use std::collections::BTreeMap;

use nori_acp_kit::AgentError;
use nori_acp_kit::Choice;
use nori_acp_kit::MODE;
use nori_acp_kit::MODEL;
use nori_acp_kit::Select;
use nori_acp_kit::THOUGHT_LEVEL;
use nori_acp_kit::schema::SessionConfigOption;
use nori_acp_kit::schema::SessionConfigOptionCategory;
use nori_protocol::AppliesAt;
use nori_protocol::Consequence;
use nori_protocol::DialEffect;

use crate::flavor::Dial;
use crate::flavor::ModelInfo;
use crate::flavor::Workspace;
use crate::store::SessionConfig;

/// The dials as they stand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Dials {
    pub(crate) model: String,
    pub(crate) effort: Option<String>,
    pub(crate) mode: Option<String>,
    /// The flavor's dials, by option id.
    pub(crate) extra: BTreeMap<String, String>,
}

impl Dials {
    /// The dials a session starts with: what was recorded where the workspace still offers it,
    /// else the workspace's defaults.
    pub(crate) fn initial(
        workspace: &Workspace,
        recorded: &SessionConfig,
    ) -> Result<Self, AgentError> {
        let mode = recorded
            .mode
            .clone()
            .filter(|mode| workspace.modes.iter().any(|known| &known.id == mode))
            .or_else(|| workspace.default_mode.clone())
            .or_else(|| workspace.modes.first().map(|mode| mode.id.clone()));
        let mode_model = mode
            .as_ref()
            .and_then(|id| workspace.modes.iter().find(|known| &known.id == id))
            .and_then(|mode| mode.model.clone());
        let model = recorded
            .model
            .clone()
            .filter(|model| workspace.model(model).is_some())
            .or(mode_model.filter(|model| workspace.model(model).is_some()))
            .or_else(|| workspace.default_model.clone())
            .or_else(|| workspace.models.first().map(|model| model.id.clone()))
            .ok_or_else(|| {
                AgentError::AuthRequired(
                    "No model is available: sign in to a provider first.".to_owned(),
                )
            })?;
        let effort = workspace
            .model(&model)
            .and_then(|info| clamp_effort(info, recorded.effort.as_deref()));
        let extra = workspace
            .dials
            .iter()
            .map(|dial| {
                let wanted = recorded.extra.get(&dial.id).map(String::as_str);
                (dial.id.clone(), settle(dial, &model, wanted))
            })
            .collect();
        Ok(Self {
            model,
            effort,
            mode,
            extra,
        })
    }

    pub(crate) fn config(&self) -> SessionConfig {
        SessionConfig {
            model: Some(self.model.clone()),
            effort: self.effort.clone(),
            mode: self.mode.clone(),
            extra: self.extra.clone(),
        }
    }

    /// The request body fields the flavor's dials set now.
    pub(crate) fn body_fields(
        &self,
        workspace: &Workspace,
    ) -> serde_json::Map<String, serde_json::Value> {
        workspace
            .dials
            .iter()
            .filter_map(|dial| {
                let value = self.extra.get(&dial.id)?;
                let choice = dial
                    .choices_for(&self.model)
                    .find(|choice| &choice.value == value)?;
                Some((dial.field.clone(), choice.body.clone()?))
            })
            .collect()
    }

    /// Keep every flavor dial on a choice the current model offers.
    fn settle_extra(&mut self, workspace: &Workspace) {
        for dial in &workspace.dials {
            let wanted = self.extra.get(&dial.id).cloned();
            let settled = settle(dial, &self.model, wanted.as_deref());
            self.extra.insert(dial.id.clone(), settled);
        }
    }

    /// Set option `id` to `value`.
    pub(crate) fn set(
        &mut self,
        workspace: &Workspace,
        id: &str,
        value: &str,
    ) -> Result<(), AgentError> {
        match id {
            MODEL => {
                let info = workspace.model(value).ok_or_else(|| {
                    AgentError::invalid(format!("{value:?} is not one of this agent's models"))
                })?;
                self.model = value.to_owned();
                self.effort = clamp_effort(info, self.effort.as_deref());
                self.settle_extra(workspace);
            }
            THOUGHT_LEVEL => {
                let info = workspace
                    .model(&self.model)
                    .ok_or_else(|| AgentError::invalid("the current model is unknown"))?;
                if !info.efforts.iter().any(|effort| effort == value) {
                    return Err(AgentError::invalid(format!(
                        "{} does not take the effort {value:?} (it takes {})",
                        info.label,
                        info.efforts.join(", ")
                    )));
                }
                self.effort = Some(value.to_owned());
            }
            MODE => {
                let mode = workspace
                    .modes
                    .iter()
                    .find(|mode| mode.id == value)
                    .ok_or_else(|| AgentError::invalid(format!("{value:?} is not a mode")))?;
                self.mode = Some(value.to_owned());
                if let Some(model) = mode
                    .model
                    .as_ref()
                    .filter(|model| workspace.model(model).is_some())
                {
                    model.clone_into(&mut self.model);
                    self.effort = workspace
                        .model(model)
                        .and_then(|info| clamp_effort(info, self.effort.as_deref()));
                    self.settle_extra(workspace);
                }
            }
            other => {
                let dial = workspace.dial(other).ok_or_else(|| {
                    AgentError::invalid(format!("unknown configuration option {other:?}"))
                })?;
                if !dial
                    .choices_for(&self.model)
                    .any(|choice| choice.value == value)
                {
                    return Err(AgentError::invalid(format!(
                        "{value:?} is not a {} the current model offers",
                        dial.label
                    )));
                }
                self.extra.insert(dial.id.clone(), value.to_owned());
            }
        }
        Ok(())
    }

    /// The options as ACP shows them.
    pub(crate) fn options(&self, workspace: &Workspace) -> Vec<SessionConfigOption> {
        let mut options = Vec::new();
        let current_window = workspace
            .model(&self.model)
            .and_then(|current| current.context_window);
        let models = workspace
            .models
            .iter()
            .map(|model| {
                let mut costs = Vec::new();
                if model.id != self.model {
                    costs.push(Consequence::PromptCacheRestarts);
                    if let (Some(from), Some(to)) = (current_window, model.context_window)
                        && to < from
                    {
                        costs.push(Consequence::ContextShrinks { from, to });
                    }
                }
                let effect = DialEffect {
                    when: AppliesAt::NextStep,
                    costs,
                };
                let choice =
                    Choice::new(model.id.clone(), model.label.clone()).with_meta(effect.to_meta());
                match &model.description {
                    Some(description) => choice.describe(description.clone()),
                    None => choice,
                }
            })
            .collect();
        options.push(Select::model(models, self.model.clone()).to_acp());
        if let (Some(info), Some(effort)) = (workspace.model(&self.model), &self.effort) {
            // Whether the connection thinks with an Anthropic budget (which restarts the prompt
            // cache when it changes) is not known here, so no cost is declared.
            let declared = DialEffect {
                when: AppliesAt::NextStep,
                costs: Vec::new(),
            };
            let efforts = info
                .efforts
                .iter()
                .map(|effort| {
                    Choice::new(effort.clone(), effort_label(effort)).with_meta(declared.to_meta())
                })
                .collect();
            options.push(Select::thought_level(efforts, effort.clone()).to_acp());
        }
        if let Some(mode) = &self.mode {
            let modes = workspace
                .modes
                .iter()
                .map(|mode| {
                    let choice = Choice::new(mode.id.clone(), mode.label.clone()).with_meta(
                        DialEffect {
                            when: AppliesAt::Now,
                            costs: Vec::new(),
                        }
                        .to_meta(),
                    );
                    match &mode.description {
                        Some(description) => choice.describe(description.clone()),
                        None => choice,
                    }
                })
                .collect();
            options.push(
                Select::new(
                    MODE,
                    "Mode",
                    SessionConfigOptionCategory::Mode,
                    modes,
                    mode.clone(),
                )
                .to_acp(),
            );
        }
        for dial in &workspace.dials {
            let choices = dial
                .choices_for(&self.model)
                .map(|choice| {
                    let shown = Choice::new(choice.value.clone(), choice.label.clone());
                    match &choice.description {
                        Some(description) => shown.describe(description.clone()),
                        None => shown,
                    }
                })
                .collect();
            let current = self
                .extra
                .get(&dial.id)
                .cloned()
                .unwrap_or_else(|| dial.default.clone());
            let select = Select::new(
                dial.id.clone(),
                dial.label.clone(),
                dial.category.clone(),
                choices,
                current,
            );
            options.push(
                match &dial.description {
                    Some(description) => select.describe(description.clone()),
                    None => select,
                }
                .to_acp(),
            );
        }
        options
    }
}

/// The choice `dial` takes for `model` when `wanted` was asked for: `wanted` when the model
/// offers it, else the dial's default.
fn settle(dial: &Dial, model: &str, wanted: Option<&str>) -> String {
    wanted
        .filter(|wanted| {
            dial.choices_for(model)
                .any(|choice| choice.value == *wanted)
        })
        .unwrap_or(&dial.default)
        .to_owned()
}

fn effort_label(effort: &str) -> String {
    let mut chars = effort.chars();
    match chars.next() {
        Some(first) => first
            .to_uppercase()
            .chain(chars)
            .collect::<String>()
            .replace('_', " "),
        None => String::new(),
    }
}

/// The effort `model` runs with when `wanted` was asked for: `wanted` itself when the model takes
/// it, else the nearest it takes (by the usual order of effort names), else its default.
pub(crate) fn clamp_effort(model: &ModelInfo, wanted: Option<&str>) -> Option<String> {
    if model.efforts.is_empty() {
        return None;
    }
    let fallback = model
        .default_effort
        .clone()
        .filter(|effort| model.efforts.contains(effort))
        .or_else(|| model.efforts.get(model.efforts.len() / 2).cloned());
    let Some(wanted) = wanted else {
        return fallback;
    };
    if model.efforts.iter().any(|effort| effort == wanted) {
        return Some(wanted.to_owned());
    }
    const ORDER: [&str; 8] = [
        "none", "minimal", "low", "medium", "high", "xhigh", "max", "maximum",
    ];
    let rank = |effort: &str| ORDER.iter().position(|known| *known == effort);
    match rank(wanted) {
        Some(target) => model
            .efforts
            .iter()
            .filter_map(|effort| rank(effort).map(|position| (position.abs_diff(target), effort)))
            .min_by_key(|(distance, _)| *distance)
            .map(|(_, effort)| effort.clone())
            .or(fallback),
        None => fallback,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn model(id: &str, efforts: &[&str]) -> ModelInfo {
        ModelInfo {
            id: id.to_owned(),
            label: id.to_owned(),
            description: None,
            context_window: None,
            max_output_tokens: None,
            efforts: efforts.iter().map(|effort| (*effort).to_owned()).collect(),
            default_effort: None,
            images: true,
        }
    }

    #[test]
    fn switching_models_keeps_only_an_effort_the_new_model_takes() {
        let workspace = Workspace {
            models: vec![
                model("openai/a", &["minimal", "low", "medium", "high", "xhigh"]),
                model("openai/b", &["low", "medium", "high"]),
                model("local/c", &[]),
            ],
            ..Workspace::default()
        };
        let mut dials = Dials::initial(&workspace, &SessionConfig::default())
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(dials.effort.as_deref(), Some("medium"));
        let steps = [
            (THOUGHT_LEVEL, "xhigh", Some("xhigh")),
            (MODEL, "openai/b", Some("high")),
            (MODEL, "local/c", None),
        ];
        for (id, value, effort) in steps {
            dials
                .set(&workspace, id, value)
                .unwrap_or_else(|error| panic!("{error}"));
            assert_eq!(dials.effort.as_deref(), effort);
        }
        assert!(dials.set(&workspace, THOUGHT_LEVEL, "high").is_err());
    }

    #[test]
    fn a_flavor_dial_offers_per_model_choices_and_falls_back_on_a_switch() {
        let choice = |value: &str, models: &[&str]| crate::flavor::DialChoice {
            value: value.to_owned(),
            label: value.to_owned(),
            description: None,
            body: (value != "default").then(|| serde_json::json!(value)),
            models: models.iter().map(|model| (*model).to_owned()).collect(),
        };
        let workspace = Workspace {
            models: vec![model("a", &[]), model("b", &[])],
            dials: vec![Dial {
                id: "service_tier".to_owned(),
                label: "Service tier".to_owned(),
                description: None,
                category: SessionConfigOptionCategory::ModelConfig,
                field: "service_tier".to_owned(),
                choices: vec![
                    choice("default", &[]),
                    choice("priority", &["a"]),
                    choice("flex", &[]),
                ],
                default: "default".to_owned(),
            }],
            ..Workspace::default()
        };
        let recorded = SessionConfig {
            extra: BTreeMap::from([("service_tier".to_owned(), "priority".to_owned())]),
            ..SessionConfig::default()
        };
        let mut dials =
            Dials::initial(&workspace, &recorded).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            dials.body_fields(&workspace),
            serde_json::Map::from_iter([(
                "service_tier".to_owned(),
                serde_json::json!("priority")
            )])
        );
        assert!(dials.set(&workspace, "service_tier", "nope").is_err());
        dials
            .set(&workspace, MODEL, "b")
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            dials.extra.get("service_tier").map(String::as_str),
            Some("default")
        );
        assert!(dials.body_fields(&workspace).is_empty());
        assert!(dials.set(&workspace, "service_tier", "priority").is_err());
        dials
            .set(&workspace, "service_tier", "flex")
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            dials.config().extra.get("service_tier").map(String::as_str),
            Some("flex")
        );
    }

    #[test]
    fn every_choice_declares_what_choosing_it_does() {
        use nori_acp_kit::schema::SessionConfigKind;
        use nori_acp_kit::schema::SessionConfigSelectOptions;

        let sized = |id: &str, window: Option<i64>| ModelInfo {
            context_window: window,
            ..model(id, &["low", "high"])
        };
        let workspace = Workspace {
            models: vec![
                sized("a", Some(1_000_000)),
                sized("b", Some(200_000)),
                sized("c", None),
            ],
            modes: vec![crate::flavor::Mode {
                id: "build".to_owned(),
                label: "Build".to_owned(),
                description: None,
                rules: Vec::new(),
                prompt: None,
                model: None,
            }],
            default_mode: Some("build".to_owned()),
            ..Workspace::default()
        };
        let dials = Dials::initial(&workspace, &SessionConfig::default())
            .unwrap_or_else(|error| panic!("{error}"));
        let declared = |id: &str| -> Vec<(String, Option<DialEffect>)> {
            let options = dials.options(&workspace);
            let option = options
                .iter()
                .find(|option| option.id.to_string() == id)
                .unwrap_or_else(|| panic!("no {id} option"));
            let SessionConfigKind::Select(select) = &option.kind else {
                panic!("{id} is not a select");
            };
            let SessionConfigSelectOptions::Ungrouped(choices) = &select.options else {
                panic!("{id} is grouped");
            };
            choices
                .iter()
                .map(|choice| (choice.value.to_string(), DialEffect::of_choice(choice)))
                .collect()
        };
        let effect = |when, costs| Some(DialEffect { when, costs });
        assert_eq!(
            declared(MODEL),
            vec![
                ("a".to_owned(), effect(AppliesAt::NextStep, vec![])),
                (
                    "b".to_owned(),
                    effect(
                        AppliesAt::NextStep,
                        vec![
                            Consequence::PromptCacheRestarts,
                            Consequence::ContextShrinks {
                                from: 1_000_000,
                                to: 200_000
                            }
                        ]
                    )
                ),
                (
                    "c".to_owned(),
                    effect(AppliesAt::NextStep, vec![Consequence::PromptCacheRestarts])
                ),
            ]
        );
        assert_eq!(
            declared(THOUGHT_LEVEL),
            vec![
                ("low".to_owned(), effect(AppliesAt::NextStep, vec![])),
                ("high".to_owned(), effect(AppliesAt::NextStep, vec![])),
            ]
        );
        assert_eq!(
            declared(MODE),
            vec![("build".to_owned(), effect(AppliesAt::Now, vec![]))]
        );
    }
}
