//! What `agent_capabilities` answers: the agents a session can delegate to, with what each offers.

use std::sync::atomic::Ordering;

use serde::Serialize;

use super::Inner;

/// The agents a session can delegate to, for `agent_capabilities`.
#[derive(Debug, Serialize)]
pub(super) struct Capabilities {
    pub(super) can_delegate: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) reason: Option<String>,
    pub(super) depth: i64,
    pub(super) running_tasks: i64,
    pub(super) agents: Vec<AgentEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct AgentEntry {
    /// The value to pass as `agent`.
    pub(super) agent: String,
    pub(super) name: String,
    /// The agent this session runs on.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) is_yours: bool,
    /// The model a child gets when `model` is not given, when Nori is configured with one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) default_model: Option<String>,
    /// Models the agent offered when Nori last ran it; empty when it has not run yet. Any model
    /// it offers can be passed; an unknown one fails with the list.
    pub(super) models: Vec<ModelEntry>,
    /// How long ago, in seconds, Nori last read what the agent offers; absent when it never has.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) read_seconds_ago: Option<i64>,
    /// The agent's other settings that `options` can set, with their values.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) settings: Vec<SettingEntry>,
}

#[derive(Debug, Serialize)]
pub(super) struct ModelEntry {
    pub(super) model: String,
    pub(super) name: String,
    /// Why this model should not be relied on: it failed as unavailable, or the agent did not
    /// advertise it when last read. Absent for a model the agent advertised and that works.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) caveat: Option<String>,
}

#[derive(Debug, Serialize)]
pub(super) struct SettingEntry {
    pub(super) setting: String,
    pub(super) name: String,
    pub(super) values: Vec<String>,
}

impl Inner {
    pub(super) fn capabilities(&self) -> Capabilities {
        let config = &self.seed.config;
        let agents = crate::recovery::installed_agents(Some(&config.active_agent))
            .into_iter()
            .map(|agent| AgentEntry {
                models: listed_models(&config.nori_home, &agent.agent_name),
                read_seconds_ago: Some(
                    super::catalog::remembered(&config.nori_home, &agent.agent_name).refreshed_at,
                )
                .filter(|refreshed_at| *refreshed_at > 0)
                .map(|refreshed_at| super::catalog::now_seconds() - refreshed_at),
                settings: super::catalog::remembered(&config.nori_home, &agent.agent_name)
                    .settings
                    .into_iter()
                    .map(|setting| SettingEntry {
                        setting: setting.id,
                        name: setting.name,
                        values: setting
                            .values
                            .into_iter()
                            .map(|value| value.value)
                            .collect(),
                    })
                    .collect(),
                default_model: config.default_models.get(&agent.agent_name).cloned(),
                is_yours: agent.agent_name == config.active_agent,
                name: agent.display_name,
                agent: agent.agent_name,
            })
            .collect();
        Capabilities {
            can_delegate: true,
            reason: None,
            depth: self.seed.depth,
            running_tasks: self.seed.tree.running.load(Ordering::Acquire),
            agents,
        }
    }
}

/// The models `agent` offered when it last ran, then any others Nori knows it can be given.
/// Each carries a caveat when it is not something to rely on.
fn listed_models(home: &std::path::Path, agent: &str) -> Vec<ModelEntry> {
    let mut models: Vec<ModelEntry> = super::catalog::offered_models(home, agent)
        .into_iter()
        .map(|model| ModelEntry {
            caveat: match model.availability {
                crate::recovery::Availability::Runnable => None,
                crate::recovery::Availability::Unavailable { reason } => {
                    Some(format!("unavailable: {reason}"))
                }
            },
            model: model.value,
            name: model.name,
        })
        .collect();
    let known = crate::registry::AgentKind::from_slug(agent)
        .map(crate::registry::AgentKind::other_models)
        .unwrap_or_default();
    for other in known {
        if !models.iter().any(|model| model.model == other.id) {
            models.push(ModelEntry {
                model: other.id.to_owned(),
                name: other.label.to_owned(),
                caveat: Some("not advertised by the agent when last read".to_owned()),
            });
        }
    }
    models
}

impl super::Orchestrator {
    /// Put the cached provider catalog in the first prompt without probing agents or
    /// changing the static host-tool instructions used by prewarmed processes.
    pub(crate) async fn initial_catalog_context(
        &self,
        context: Option<String>,
        workflow: &tokio::sync::Mutex<crate::backend::workflow::WorkflowState>,
        tools_available: bool,
    ) -> Option<String> {
        let workflow = workflow.lock().await;
        let policy = workflow
            .active
            .as_ref()
            .and_then(|workflow| workflow.tools.as_ref());
        if !tools_available || !catalog_tools_enabled(policy) {
            return context;
        }
        let Ok(catalog) = serde_json::to_string(&self.inner.capabilities()) else {
            return context;
        };
        let catalog = format!(
            "<nori_agent_catalog>\nCached configured delegation capabilities: {catalog}\nModels and settings may be stale; an empty model list means unprobed, not unavailable. Use agent_capabilities to refresh before choosing a provider or model.\n</nori_agent_catalog>"
        );
        Some(match context {
            Some(context) => format!("{context}\n\n{catalog}"),
            None => catalog,
        })
    }
}

fn catalog_tools_enabled(policy: Option<&nori_workflow::ToolPolicy>) -> bool {
    ["agent_capabilities", "delegate_task"].iter().all(|name| {
        policy.is_none_or(|policy| {
            policy
                .enabled
                .as_ref()
                .is_none_or(|enabled| enabled.iter().any(|enabled| enabled.as_str() == *name))
                && !policy
                    .disabled
                    .iter()
                    .any(|disabled| disabled.as_str() == *name)
        })
    })
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn initial_context_includes_cached_capabilities_only_when_tools_are_available() {
        let (events, _receiver) = tokio::sync::mpsc::channel(8);
        let seed = super::super::Seed::root(
            std::sync::Arc::new(crate::config::NoriConfig::default()),
            "test".into(),
        );
        let (orchestrator, _signals) = super::super::Orchestrator::new(seed, events);
        let workflow = tokio::sync::Mutex::new(crate::backend::workflow::WorkflowState::default());
        let initial = Some("existing context".to_owned());
        assert_eq!(
            orchestrator
                .initial_catalog_context(initial.clone(), &workflow, false)
                .await,
            initial
        );
        let context = orchestrator
            .initial_catalog_context(initial, &workflow, true)
            .await
            .unwrap();
        assert!(context.starts_with("existing context\n\n<nori_agent_catalog>"));
        let catalog = context
            .lines()
            .find_map(|line| line.strip_prefix("Cached configured delegation capabilities: "))
            .unwrap();
        let catalog: serde_json::Value = serde_json::from_str(catalog).unwrap();
        assert_eq!(catalog["can_delegate"], true);
        assert_eq!(catalog["depth"], 0);
        assert!(catalog["agents"].is_array());
    }

    #[test]
    fn cached_catalog_requires_both_delegation_tools() {
        assert!(catalog_tools_enabled(None));
        let mut policy = nori_workflow::ToolPolicy::default();
        assert!(catalog_tools_enabled(Some(&policy)));
        policy.disabled.push("agent_capabilities".into());
        assert!(!catalog_tools_enabled(Some(&policy)));
        policy.disabled.clear();
        policy.enabled = Some(vec!["agent_capabilities".into()]);
        assert!(!catalog_tools_enabled(Some(&policy)));
        policy
            .enabled
            .as_mut()
            .unwrap()
            .push("delegate_task".into());
        assert!(catalog_tools_enabled(Some(&policy)));
    }
}
