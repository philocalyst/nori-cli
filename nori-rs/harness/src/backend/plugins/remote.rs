//! The plugin API on the remote-control channel (`docs/remote-control.md`): `_nori/plugins` lists
//! what the session offers and `_nori/plugin.run` runs a command, both over the ACP extension
//! methods a remote client already speaks.

use std::collections::BTreeMap;

use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::types::PluginError;
use super::types::PluginInfo;
use crate::runtime::HarnessHandle;

/// The method that lists the plugins.
pub const PLUGINS_METHOD: &str = "_nori/plugins";
/// The method that runs a plugin's command.
pub const PLUGIN_RUN_METHOD: &str = "_nori/plugin.run";

/// The answer to `_nori/plugins`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginsResponse {
    pub plugins: Vec<PluginInfo>,
}

/// The parameters of `_nori/plugin.run`. The answer is a [`super::PluginRun`]; an error carries
/// the [`PluginError`] as its `data`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginRunRequest {
    pub plugin: String,
    pub command: String,
    #[serde(default)]
    pub arguments: BTreeMap<String, String>,
}

/// Answer an extension request of the plugin API, or say that `method` is not one.
pub async fn answer(
    handle: &HarnessHandle,
    method: &str,
    params: serde_json::Value,
) -> Option<Result<serde_json::Value, acp::Error>> {
    match method {
        PLUGINS_METHOD => Some(
            handle
                .plugins()
                .await
                .map_err(|error| acp::Error::internal_error().data(format!("{error:#}")))
                .and_then(|plugins| to_json(&PluginsResponse { plugins })),
        ),
        PLUGIN_RUN_METHOD => Some(run(handle, params).await),
        _ => None,
    }
}

async fn run(
    handle: &HarnessHandle,
    params: serde_json::Value,
) -> Result<serde_json::Value, acp::Error> {
    let request: PluginRunRequest = serde_json::from_value(params)
        .map_err(|error| acp::Error::invalid_params().data(error.to_string()))?;
    match handle
        .run_plugin(&request.plugin, &request.command, request.arguments)
        .await
    {
        Ok(run) => to_json(&run),
        Err(error) => {
            let mut failure = match &error {
                PluginError::StepFailed { .. } | PluginError::SessionClosed => {
                    acp::Error::internal_error()
                }
                PluginError::UnknownPlugin { .. }
                | PluginError::UnknownCommand { .. }
                | PluginError::UnknownArgument { .. }
                | PluginError::MissingArgument { .. }
                | PluginError::UnknownPlaceholder { .. }
                | PluginError::UnknownAction { .. }
                | PluginError::BadArgument { .. } => acp::Error::invalid_params(),
            };
            failure.message = error.to_string();
            failure.data = serde_json::to_value(&error).ok();
            Err(failure)
        }
    }
}

fn to_json(value: &impl Serialize) -> Result<serde_json::Value, acp::Error> {
    serde_json::to_value(value)
        .map_err(|error| acp::Error::internal_error().data(error.to_string()))
}
