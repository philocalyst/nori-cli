//! The gateway's models as the engine runs them: which wire each speaks, at which address, with
//! which efforts.

use nori_engine::ModelInfo;
use nori_llm::Wire;

use crate::catalog::CatalogModel;
use crate::catalog::Gateway;

/// The gateway's own address, when models.dev does not name one.
const GATEWAY_API: &str = "https://opencode.ai/zen/v1";

/// One model, ready to connect to.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ModelEntry {
    pub(crate) info: ModelInfo,
    /// The model as the gateway names it.
    pub(crate) wire_model: String,
    pub(crate) wire: Wire,
    pub(crate) base_url: String,
    pub(crate) reasoning_field: Option<String>,
    release_date: String,
}

/// The wire an AI SDK package speaks.
fn package_wire(package: &str) -> Wire {
    match package.trim_end_matches('/') {
        "@ai-sdk/anthropic" => Wire::AnthropicMessages,
        "@ai-sdk/openai" => Wire::OpenAiResponses,
        _ => Wire::OpenAiChat,
    }
}

fn default_efforts(wire: Wire, reasoning: bool) -> (Vec<String>, Option<String>) {
    if !reasoning {
        return (Vec::new(), None);
    }
    let list = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
    match wire {
        Wire::AnthropicMessages => (
            list(&["none", "low", "medium", "high", "max"]),
            Some("none".to_owned()),
        ),
        Wire::OpenAiResponses => (list(&["low", "medium", "high"]), Some("medium".to_owned())),
        Wire::OpenAiChat => (Vec::new(), None),
    }
}

/// Every model an agent can run on the gateway, newest first. A model without tools cannot work
/// as an agent, and a deprecated one is on its way out; neither is offered.
pub(crate) fn models(gateway: &Gateway) -> Vec<ModelEntry> {
    let mut models: Vec<ModelEntry> = gateway
        .models
        .iter()
        .filter(|(_, model)| model.tool_call && !model.deprecated)
        .map(|(key, model)| entry(gateway, key, model))
        .collect();
    models.sort_by(|left, right| {
        right
            .release_date
            .cmp(&left.release_date)
            .then_with(|| left.info.label.cmp(&right.info.label))
    });
    models
}

/// `model` as the engine runs it.
fn entry(gateway: &Gateway, key: &str, model: &CatalogModel) -> ModelEntry {
    let package = model
        .package
        .as_deref()
        .or(gateway.package.as_deref())
        .unwrap_or("@ai-sdk/openai-compatible");
    let wire = package_wire(package);
    let base_url = model
        .api
        .as_deref()
        .or(gateway.api.as_deref())
        .unwrap_or(GATEWAY_API)
        .to_owned();
    let (efforts, default_effort) = if model.efforts.is_empty() {
        default_efforts(wire, model.reasoning)
    } else {
        let default = ["medium", "high"]
            .iter()
            .find(|effort| model.efforts.iter().any(|known| known == *effort))
            .map(|effort| (*effort).to_owned());
        (model.efforts.clone(), default)
    };
    ModelEntry {
        info: ModelInfo {
            // As Nori's OpenCode always named the gateway's models, so a session keeps its own.
            id: format!("opencode/{key}"),
            label: model.name.clone(),
            description: None,
            context_window: model.context,
            max_output_tokens: model.output,
            efforts,
            default_effort,
            images: model.images,
        },
        wire_model: model.id.clone(),
        wire,
        base_url,
        reasoning_field: model.reasoning_field.clone(),
        release_date: model.release_date.clone(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pretty_assertions::assert_eq;

    use super::*;

    fn model(name: &str, package: Option<&str>, released: &str) -> CatalogModel {
        CatalogModel {
            id: name.to_owned(),
            name: name.to_owned(),
            reasoning: true,
            tool_call: true,
            package: package.map(ToOwned::to_owned),
            release_date: released.to_owned(),
            ..CatalogModel::default()
        }
    }

    #[test]
    fn each_model_speaks_its_packages_wire_and_the_newest_come_first() {
        let gateway = Gateway {
            name: "OpenCode Zen".to_owned(),
            package: Some("@ai-sdk/openai-compatible".to_owned()),
            api: Some("https://opencode.ai/zen/v1".to_owned()),
            models: BTreeMap::from([
                (
                    "claude-x".to_owned(),
                    model("claude-x", Some("@ai-sdk/anthropic"), "2026-10-01"),
                ),
                (
                    "gpt-x".to_owned(),
                    model("gpt-x", Some("@ai-sdk/openai"), "2026-11-01"),
                ),
                ("kimi-x".to_owned(), model("kimi-x", None, "2026-01-01")),
                (
                    "old".to_owned(),
                    CatalogModel {
                        deprecated: true,
                        ..model("old", None, "2027-01-01")
                    },
                ),
            ]),
        };
        let offered: Vec<(String, Wire, Option<String>)> = models(&gateway)
            .into_iter()
            .map(|entry| (entry.info.id, entry.wire, entry.info.default_effort))
            .collect();
        assert_eq!(
            offered,
            vec![
                (
                    "opencode/gpt-x".to_owned(),
                    Wire::OpenAiResponses,
                    Some("medium".to_owned())
                ),
                (
                    "opencode/claude-x".to_owned(),
                    Wire::AnthropicMessages,
                    Some("none".to_owned())
                ),
                ("opencode/kimi-x".to_owned(), Wire::OpenAiChat, None),
            ]
        );
    }
}
