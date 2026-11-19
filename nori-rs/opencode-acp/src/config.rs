//! OpenCode's model and effort selections as ACP session config options.

use acp::schema::v1::SessionConfigOption;
use nori_acp_kit::acp;
use reqwest::Method;
use serde_json::Value;
use serde_json::json;

use crate::OpenCodeServer;

pub(crate) struct Catalog {
    models: Vec<Value>,
    current: Value,
    agents: Vec<Value>,
    current_agent: String,
    commands: Vec<Value>,
}

impl Catalog {
    pub(crate) async fn load(server: &OpenCodeServer, existing: &Value) -> acp::Result<Self> {
        let models = server
            .request_catalog(&["model"])
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let default = server
            .request_catalog(&["model", "default"])
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let models = models["data"].as_array().cloned().unwrap_or_default();
        let default = &default["data"];
        let provider = default["providerID"].as_str().ok_or_else(|| {
            acp::Error::internal_error().data("OpenCode default model has no provider")
        })?;
        let id = default["id"]
            .as_str()
            .ok_or_else(|| acp::Error::internal_error().data("OpenCode default model has no id"))?;
        // A fresh v2 location can briefly return an empty agent catalog while plugins load.
        let mut agents = Vec::new();
        for _ in 0..100 {
            let response = server
                .request_catalog(&["agent"])
                .await
                .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
            agents = response["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|agent| agent["mode"] != "subagent" && agent["hidden"] != true)
                .cloned()
                .collect();
            if !agents.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let default_agent = agents
            .first()
            .and_then(|agent| agent["id"].as_str())
            .ok_or_else(|| {
                acp::Error::internal_error().data("OpenCode agent catalog did not become ready")
            })?;
        let current_agent = existing["agent"]
            .as_str()
            .unwrap_or(default_agent)
            .to_owned();
        let current = if existing["model"].is_object() {
            existing["model"].clone()
        } else {
            json!({"providerID":provider,"id":id})
        };
        let response = server
            .request_catalog(&["command"])
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        let mut commands = response["data"].as_array().into_iter().flatten().filter_map(|command| {
            let name = command["name"].as_str()?;
            if name == "compact" { return None; }
            Some(json!({"name":name,"description":command["description"].as_str().unwrap_or_default()}))
        }).collect::<Vec<_>>();
        commands.push(json!({"name":"compact","description":"Compact the session"}));
        Ok(Self {
            models,
            current,
            agents,
            current_agent,
            commands,
        })
    }

    pub(crate) fn commands(&self) -> &[Value] {
        &self.commands
    }

    /// The selected model as `provider/id`, the form the model option uses.
    pub(crate) fn current_model(&self) -> Option<String> {
        Some(format!(
            "{}/{}",
            self.current["providerID"].as_str()?,
            self.current["id"].as_str()?
        ))
    }

    pub(crate) fn context_limit(&self) -> Option<i64> {
        self.models.iter().find(|model| {
            model["providerID"] == self.current["providerID"] && model["id"] == self.current["id"]
        })?["limit"]["context"]
            .as_i64()
    }

    pub(crate) fn options(&self) -> acp::Result<Vec<SessionConfigOption>> {
        let provider = self.current["providerID"].as_str().unwrap_or_default();
        let id = self.current["id"].as_str().unwrap_or_default();
        let options = self
            .models
            .iter()
            .filter_map(|model| {
                if model["enabled"].as_bool() == Some(false) {
                    return None;
                }
                let provider = model["providerID"].as_str()?;
                let id = model["id"].as_str()?;
                let name = model["name"].as_str().unwrap_or(id);
                Some(
                    json!({"value":format!("{provider}/{id}"),"name":format!("{provider}/{name}")}),
                )
            })
            .collect::<Vec<_>>();
        let mut result = vec![
            json!({"id":"model","name":"Model","category":"model","type":"select","currentValue":format!("{provider}/{id}"),"options":options}),
        ];
        if let Some(variants) = self
            .models
            .iter()
            .find(|item| item["providerID"] == provider && item["id"] == id)
            .and_then(|item| item["variants"].as_array())
            && !variants.is_empty()
        {
            let mut options = variants
                .iter()
                .filter_map(|variant| variant["id"].as_str())
                .map(|variant| json!({"value":variant,"name":variant}))
                .collect::<Vec<_>>();
            if !options.iter().any(|option| option["value"] == "default") {
                options.push(json!({"value":"default","name":"Default"}));
            }
            result.push(json!({"id":"effort","name":"Effort","category":"thought_level","type":"select","currentValue":self.current["variant"].as_str().unwrap_or("default"),"options":options}));
        }
        let modes = self
            .agents
            .iter()
            .filter_map(|agent| {
                Some(json!({"value":agent["id"].as_str()?,"name":agent["name"].as_str()?}))
            })
            .collect::<Vec<_>>();
        result.push(json!({"id":"mode","name":"Session Mode","category":"mode","type":"select","currentValue":self.current_agent,"options":modes}));
        serde_json::from_value(Value::Array(result)).map_err(acp::Error::into_internal_error)
    }

    pub(crate) async fn set(
        &mut self,
        server: &OpenCodeServer,
        session: &str,
        config: &str,
        value: &str,
    ) -> acp::Result<Vec<SessionConfigOption>> {
        if config == "mode" {
            if !self.agents.iter().any(|agent| agent["id"] == value) {
                return Err(acp::Error::invalid_params());
            }
            server
                .request(
                    Method::POST,
                    &["session", session, "agent"],
                    Some(&json!({"agent":value})),
                )
                .await
                .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
            self.current_agent = value.to_owned();
            return self.options();
        }
        let next = match config {
            "model" => {
                let (provider, id) = value
                    .split_once('/')
                    .ok_or_else(acp::Error::invalid_params)?;
                let model = self
                    .models
                    .iter()
                    .find(|model| {
                        model["providerID"] == provider
                            && model["id"] == id
                            && model["enabled"].as_bool() != Some(false)
                    })
                    .ok_or_else(acp::Error::invalid_params)?;
                json!({"providerID":model["providerID"],"id":model["id"]})
            }
            "effort" => {
                let model = self
                    .models
                    .iter()
                    .find(|model| {
                        model["providerID"] == self.current["providerID"]
                            && model["id"] == self.current["id"]
                    })
                    .ok_or_else(acp::Error::invalid_params)?;
                if value != "default"
                    && !model["variants"].as_array().is_some_and(|variants| {
                        variants.iter().any(|variant| variant["id"] == value)
                    })
                {
                    return Err(acp::Error::invalid_params());
                }
                let mut next = self.current.clone();
                next["variant"] = value.into();
                next
            }
            _ => return Err(acp::Error::invalid_params()),
        };
        server
            .request(
                Method::POST,
                &["session", session, "model"],
                Some(&json!({"model":next})),
            )
            .await
            .map_err(|e| acp::Error::internal_error().data(e.to_string()))?;
        self.current = next;
        self.options()
    }
}
