//! The agent catalog as the rest of Nori reads it: what each installed agent offers, refreshed in
//! the background as the user starts typing.

use std::sync::Arc;
use std::time::Duration;

use super::catalog;
use super::probe;

/// Agents a typing user may soon delegate to are re-read when their entry is this old.
pub(super) const CATALOG_FRESH_FOR: Duration = Duration::from_secs(600);

/// Re-read, in the background, what each installed agent offers, for agents not read in the last
/// ten minutes: called as the user starts typing, so delegation sees current models.
pub fn refresh_agent_catalog(config: Arc<crate::config::NoriConfig>, cli_version: String) {
    tokio::spawn(probe::refresh(config, cli_version, CATALOG_FRESH_FOR));
}

/// Refresh installed agents except for the one whose primary startup session is being prepared.
/// That session records its advertised configuration directly when it starts.
pub fn refresh_agent_catalog_except(
    config: Arc<crate::config::NoriConfig>,
    cli_version: String,
    excluded_agent: String,
) {
    tokio::spawn(probe::refresh_excluding(
        config,
        cli_version,
        CATALOG_FRESH_FOR,
        Some(excluded_agent),
    ));
}

/// A model previously advertised by an agent in an ACP session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAgentModel {
    pub value: String,
    pub name: String,
}

/// Return only models actually advertised by this agent in a previous session.
pub fn cached_agent_models(home: &std::path::Path, agent: &str) -> Vec<CachedAgentModel> {
    catalog::remembered(home, agent)
        .models
        .into_iter()
        .map(|model| CachedAgentModel {
            value: model.value,
            name: model.name,
        })
        .collect()
}
