//! The choices an agent advertises through its select config options: its models, and its
//! other settings such as reasoning effort or mode.

use nori_protocol::acp::v1 as acp;

/// One model an agent offers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Model {
    /// The value the agent selects it by.
    pub(super) value: String,
    /// What the agent calls it.
    pub(super) name: String,
}

/// One select option: its id and name, the selected choice, and every choice it offers. For the
/// model option the choices are models.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Models {
    pub(super) config_id: String,
    pub(super) name: String,
    pub(super) current: String,
    pub(super) offered: Vec<Model>,
}

impl Models {
    /// The model option among `options`, if the agent has one.
    pub(super) fn of(options: &[acp::SessionConfigOption]) -> Option<Self> {
        options
            .iter()
            .find(|option| {
                option.category == Some(acp::SessionConfigOptionCategory::Model)
                    || option.id.to_string() == "model"
            })
            .and_then(Self::from_option)
    }

    /// The select option `setting` names, by id or name, ignoring case and `_`, `-`, and spaces,
    /// or by the typed names `model_mode` and `effort`.
    pub(super) fn named(options: &[acp::SessionConfigOption], setting: &str) -> Option<Self> {
        nori_protocol::find_setting(options, setting).and_then(Self::from_option)
    }

    /// Every select option other than the model, for `agent_capabilities`.
    pub(super) fn settings(options: &[acp::SessionConfigOption]) -> Vec<Self> {
        options
            .iter()
            .filter(|option| {
                option.category != Some(acp::SessionConfigOptionCategory::Model)
                    && option.id.to_string() != "model"
            })
            .filter_map(Self::from_option)
            .collect()
    }

    fn from_option(option: &acp::SessionConfigOption) -> Option<Self> {
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let offered = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .collect(),
            // The schema's option layouts are not exhaustive; an unknown one offers nothing
            // Nori can read.
            _ => Vec::new(),
        }
        .into_iter()
        .map(|option| Model {
            value: option.value.to_string(),
            name: option.name.clone(),
        })
        .collect();
        Some(Self {
            config_id: option.id.to_string(),
            name: option.name.clone(),
            current: select.current_value.to_string(),
            offered,
        })
    }

    pub(super) fn find(&self, requested: &str) -> Option<&Model> {
        find(&self.offered, requested)
    }

    pub(super) fn describe(&self) -> String {
        describe(&self.offered)
    }
}

/// The offered models as `value (name)`, for an error the parent agent can act on.
pub(super) fn describe(offered: &[Model]) -> String {
    if offered.is_empty() {
        return "no models it lists".to_owned();
    }
    offered
        .iter()
        .map(|model| {
            if model.name == model.value {
                format!("`{}`", model.value)
            } else {
                format!("`{}` ({})", model.value, model.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The model among `offered` that `requested` names: by value exactly, else by value or name
/// ignoring case.
pub(super) fn find<'a>(offered: &'a [Model], requested: &str) -> Option<&'a Model> {
    let requested = requested.trim();
    offered
        .iter()
        .find(|model| model.value == requested)
        .or_else(|| {
            offered.iter().find(|model| {
                model.value.eq_ignore_ascii_case(requested)
                    || model.name.eq_ignore_ascii_case(requested)
            })
        })
}

/// The name of the model the session runs, as its agent advertises it.
pub(super) fn current_model(options: &[acp::SessionConfigOption]) -> Option<String> {
    let models = Models::of(options)?;
    Some(
        models
            .offered
            .iter()
            .find(|model| model.value == models.current)
            .map_or(models.current.clone(), |model| model.name.clone()),
    )
}

/// Choose `requested` from the models the agent advertises, by value or by name. An agent that
/// lists none but was given the model at spawn (`injected`) runs that one.
pub(super) async fn select_model(
    harness: &crate::runtime::HarnessHandle,
    options: &[acp::SessionConfigOption],
    requested: &str,
    injected: bool,
) -> Result<String, String> {
    let Some(models) = Models::of(options) else {
        if injected || requested.trim() == "default" {
            return Ok(requested.trim().to_owned());
        }
        return Err(format!(
            "this agent does not let Nori choose its model, so `{requested}` cannot be selected; \
             delegate without `model`"
        ));
    };
    // A model forced at spawn that the agent runs, though it does not list it, is the choice.
    if models.current.eq_ignore_ascii_case(requested.trim()) {
        return Ok(current_model(options).unwrap_or_else(|| requested.to_owned()));
    }
    let Some(choice) = models.find(requested) else {
        return Err(format!(
            "this agent has no model `{requested}`; it offers: {}",
            models.describe()
        ));
    };
    if choice.value != models.current {
        let updated = harness
            .set_session_config_option(models.config_id.clone(), choice.value.clone())
            .await
            .map_err(|error| format!("the agent refused model `{}`: {error:#}", choice.value))?;
        return Ok(current_model(&updated).unwrap_or_else(|| choice.name.clone()));
    }
    Ok(choice.name.clone())
}

/// Set the other settings a task asks for, each by setting id or name and by value or name.
pub(super) async fn apply_options(
    harness: &crate::runtime::HarnessHandle,
    advertised: &[acp::SessionConfigOption],
    requested: &[(String, String)],
    may_set_mode: bool,
) -> Result<(), String> {
    for (setting, value) in requested {
        let is_mode = advertised.iter().any(|option| {
            option.category == Some(acp::SessionConfigOptionCategory::Mode)
                && Models::named(std::slice::from_ref(option), setting).is_some()
        });
        if is_mode && !may_set_mode {
            return Err(format!(
                "`{setting}` is the agent's permission mode, and you run with approvals, so a \
                 child cannot be given another"
            ));
        }
        let Some(choices) = Models::named(advertised, setting) else {
            let known: Vec<String> = Models::settings(advertised)
                .into_iter()
                .map(|setting| format!("`{}`", setting.config_id))
                .collect();
            return Err(format!(
                "this agent has no setting `{setting}`; its settings are {}",
                if known.is_empty() {
                    "none".to_owned()
                } else {
                    known.join(", ")
                }
            ));
        };
        // A mode is also asked for by what it is: `fast` finds the `on` of a fast toggle.
        let spelled = nori_protocol::find_setting(advertised, setting)
            .and_then(|option| nori_protocol::value_for(option, value));
        let Some(choice) = choices
            .find(value)
            .or_else(|| spelled.as_deref().and_then(|spelled| choices.find(spelled)))
        else {
            return Err(format!(
                "`{setting}` has no value `{value}`; it takes {}",
                choices.describe()
            ));
        };
        if choice.value != choices.current {
            harness
                .set_session_config_option(choices.config_id.clone(), choice.value.clone())
                .await
                .map_err(|error| format!("the agent refused {setting} `{value}`: {error:#}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn options() -> Vec<acp::SessionConfigOption> {
        vec![
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "gpt-5.1",
                vec![
                    acp::SessionConfigSelectOption::new("gpt-5.1", "GPT-5.1"),
                    acp::SessionConfigSelectOption::new("gpt-5.1-codex", "GPT-5.1 Codex"),
                ],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ]
    }

    #[test]
    fn a_model_option_without_a_category_still_reports_its_current_choice() {
        let mut options = options();
        options[0].category = None;
        assert_eq!(current_model(&options), Some("GPT-5.1".to_owned()));
        assert!(Models::settings(&options).is_empty());
    }

    #[test]
    fn models_are_found_by_value_or_by_name() {
        let models = Models::of(&options()).expect("model option");
        assert_eq!(models.current, "gpt-5.1");
        assert_eq!(
            models
                .find("GPT-5.1 codex")
                .map(|model| model.value.as_str()),
            Some("gpt-5.1-codex")
        );
        assert_eq!(
            models.find("gpt-5.1").map(|model| model.name.as_str()),
            Some("GPT-5.1")
        );
        assert_eq!(models.find("opus"), None);
        assert_eq!(
            models.describe(),
            "`gpt-5.1` (GPT-5.1), `gpt-5.1-codex` (GPT-5.1 Codex)"
        );
        assert_eq!(current_model(&options()).as_deref(), Some("GPT-5.1"));
        assert_eq!(current_model(&[]), None);
    }
}
