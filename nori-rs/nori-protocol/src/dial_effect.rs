//! What happens when a dial is set to a value now: when the change takes hold and what it costs.
//!
//! A dial change is never refused. The agent that offers a choice says what choosing it does
//! (`_meta.nori.dial` on the choice, parsed by [`DialEffect::of_choice`]), and the client shows
//! that where the change is made, in one quiet line. A choice that declares nothing is
//! [`DialEffect::next_turn`] with no costs.

use serde::Deserialize;
use serde::Serialize;

use crate::Dial;
use crate::acp::v1 as acp;

/// When a dial change takes hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AppliesAt {
    /// At once.
    Now,
    /// With the next request of the running turn, such as the model's next step after a tool
    /// result.
    NextStep,
    /// With the next prompt.
    NextTurn,
    /// When the running turn ends: the change is accepted now and made then.
    AfterThisTurn,
}

/// Something a dial change costs.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Consequence {
    /// The provider's prompt cache is dropped, so the next request is read at full price.
    PromptCacheRestarts,
    /// The agent session is started anew, carrying the conversation over.
    SessionRestarts,
    /// Thinking from earlier steps is not carried over.
    ThinkingReset,
    /// Requests are priced differently, as the agent words each price.
    DifferentPrice {
        from: Option<String>,
        to: Option<String>,
    },
    /// The context window is smaller, in tokens.
    ContextShrinks { from: i64, to: i64 },
}

/// What setting a dial to a value does.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DialEffect {
    pub when: AppliesAt,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub costs: Vec<Consequence>,
}

impl DialEffect {
    /// The effect of a choice that declares none: it applies with the next prompt and costs
    /// nothing Nori knows of.
    pub fn next_turn() -> Self {
        Self {
            when: AppliesAt::NextTurn,
            costs: Vec::new(),
        }
    }

    /// The effect `choice` declares in its `_meta.nori.dial`, as `{"appliesAt": .., "costs": [..]}`.
    /// None when it declares none or the declaration is not one this version reads. A cost this
    /// version does not know is left out, not an error: the rest still shows.
    pub fn of_choice(choice: &acp::SessionConfigSelectOption) -> Option<Self> {
        Self::from_meta(choice.meta.as_ref()?)
    }

    /// The effect declared in `meta` (a choice's `_meta`).
    pub fn from_meta(meta: &serde_json::Map<String, serde_json::Value>) -> Option<Self> {
        let declared = meta.get("nori")?.get("dial")?;
        let when = serde_json::from_value(declared.get("appliesAt")?.clone()).ok()?;
        let costs = declared
            .get("costs")
            .and_then(serde_json::Value::as_array)
            .map(|costs| {
                costs
                    .iter()
                    .filter_map(|cost| serde_json::from_value(cost.clone()).ok())
                    .collect()
            })
            .unwrap_or_default();
        Some(Self { when, costs })
    }

    /// The `_meta` that declares this effect on a choice.
    pub fn to_meta(&self) -> serde_json::Map<String, serde_json::Value> {
        let dial = serde_json::json!({ "appliesAt": self.when, "costs": self.costs });
        serde_json::Map::from_iter([("nori".to_owned(), serde_json::json!({ "dial": dial }))])
    }

    /// The effect of setting `dial` to `value` among `options`: what the choice declares, else
    /// [`Self::next_turn`]. None when no option of `dial` offers `value`.
    pub fn declared_for(
        options: &[acp::SessionConfigOption],
        dial: Dial,
        value: &str,
    ) -> Option<Self> {
        let option = options.iter().find(|option| dial.selects(option))?;
        let acp::SessionConfigKind::Select(select) = &option.kind else {
            return None;
        };
        let choice = match &select.options {
            acp::SessionConfigSelectOptions::Ungrouped(choices) => choices
                .iter()
                .find(|choice| choice.value.to_string() == value),
            acp::SessionConfigSelectOptions::Grouped(groups) => groups
                .iter()
                .flat_map(|group| group.options.iter())
                .find(|choice| choice.value.to_string() == value),
            _ => None,
        }?;
        Some(Self::of_choice(choice).unwrap_or_else(Self::next_turn))
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn an_effect_round_trips_through_a_choice_meta() {
        let effect = DialEffect {
            when: AppliesAt::NextStep,
            costs: vec![
                Consequence::PromptCacheRestarts,
                Consequence::DifferentPrice {
                    from: Some("$3".to_owned()),
                    to: None,
                },
                Consequence::ContextShrinks {
                    from: 1_000_000,
                    to: 200_000,
                },
            ],
        };
        let meta = effect.to_meta();
        assert_eq!(
            serde_json::Value::Object(meta),
            serde_json::json!({"nori": {"dial": {
                "appliesAt": "nextStep",
                "costs": [
                    "promptCacheRestarts",
                    {"differentPrice": {"from": "$3", "to": null}},
                    {"contextShrinks": {"from": 1000000, "to": 200000}},
                ],
            }}})
        );
        let declared = serde_json::json!({"nori": {"dial": {
            "appliesAt": "nextStep",
            "costs": [
                "promptCacheRestarts",
                {"differentPrice": {"from": "$3", "to": null}},
                {"contextShrinks": {"from": 1000000, "to": 200000}},
                "someFutureCost",
            ],
        }}});
        let choice = acp::SessionConfigSelectOption::new("a", "A")
            .meta(declared.as_object().cloned().unwrap_or_default());
        assert_eq!(DialEffect::of_choice(&choice), Some(effect.clone()));
        assert_eq!(
            serde_json::to_value(&effect).unwrap_or_default()["when"],
            "nextStep"
        );
    }

    #[test]
    fn a_choice_without_a_declaration_has_no_effect_of_its_own() {
        let bare = acp::SessionConfigSelectOption::new("a", "A");
        assert_eq!(DialEffect::of_choice(&bare), None);
        let other = acp::SessionConfigSelectOption::new("a", "A").meta(
            serde_json::json!({"nori": {"other": 1}})
                .as_object()
                .cloned()
                .unwrap_or_default(),
        );
        assert_eq!(DialEffect::of_choice(&other), None);
    }
}
