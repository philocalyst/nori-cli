//! The routes Nori can offer: installed agents and the models each last advertised.
//!
//! This is the only place that decides what may be offered. Debug fixtures never enter it
//! (unless the session itself runs on one), and a model that is known to fail is carried with
//! its reason instead of being offered as runnable.

use std::path::Path;

use super::identity::ModelIdentity;
use super::ranking::is_gateway;

/// Whether a model may be offered as something to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Runnable,
    /// It failed as unavailable, or the agent advertised a placeholder; `reason` says which.
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct CatalogModel {
    pub value: String,
    pub name: String,
    pub identity: ModelIdentity,
    pub availability: Availability,
}

#[derive(Debug, Clone)]
pub struct CatalogAgent {
    pub agent: String,
    pub display_name: String,
    pub gateway: bool,
    pub models: Vec<CatalogModel>,
}

/// Whether `agent` is a debug fixture rather than a product agent.
pub fn is_debug_fixture(agent: &str) -> bool {
    agent.starts_with("mock-")
}

/// Installed agents in display order, without their models.
///
/// Fixtures are listed only when `active_agent` is itself a fixture: a session running on one
/// is a test session, and no other session can ever be offered or fall back to one.
pub fn installed_agents(active_agent: Option<&str>) -> Vec<crate::registry::AcpAgentInfo> {
    let fixtures_allowed = active_agent.is_some_and(is_debug_fixture);
    crate::registry::list_available_agents()
        .into_iter()
        .filter(|info| info.is_installed)
        .filter(|info| fixtures_allowed || !is_debug_fixture(&info.agent_name))
        .collect()
}

/// Installed agents with their last advertised models, in display order.
pub fn route_catalog(home: &Path, active_agent: Option<&str>) -> Vec<CatalogAgent> {
    installed_agents(active_agent)
        .into_iter()
        .map(|info| CatalogAgent {
            gateway: is_gateway(&info.agent_name),
            models: crate::backend::orchestrator::offered_models(home, &info.agent_name)
                .into_iter()
                .map(|model| CatalogModel {
                    identity: ModelIdentity::of(&model.name, &model.value),
                    value: model.value,
                    name: model.name,
                    availability: model.availability,
                })
                .collect(),
            agent: info.agent_name,
            display_name: info.display_name,
        })
        .collect()
}
