//! OpenCode Models and Go catalogs, live from models.dev and OpenCode's inference model API.
//!
//! The catalog in memory is the one served. It starts as Nori's last copy on disk
//! (`<store>/models.json`), however old, and is asked for again in the background whenever a
//! session opens (at most every few minutes) and periodically while Nori runs. A catalog that
//! differs replaces it, is written back to disk, and is announced through [`Catalog::changes`] so
//! open sessions offer the new models at once. Only a first run, with no copy at all, waits for
//! models.dev. One catalog serves every agent connection of the process.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;

use crate::env::Env;

/// The provider of models.dev that is OpenCode's gateway.
const GATEWAY: &str = "opencode";
const DEFAULT_URL: &str = "https://models.dev";
const GO_MODELS_URL: &str = "https://opencode.ai/inference/go/v1/models";
/// How soon after one the opening of a session asks models.dev again.
const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often models.dev is asked while Nori runs, sessions opening or not.
const REFRESH_PERIOD: Duration = Duration::from_secs(30 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The gateway as models.dev describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Gateway {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) package: Option<String>,
    pub(crate) api: Option<String>,
    pub(crate) models: BTreeMap<String, CatalogModel>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CatalogModel {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) reasoning: bool,
    pub(crate) tool_call: bool,
    pub(crate) images: bool,
    pub(crate) temperature: bool,
    pub(crate) context: Option<i64>,
    pub(crate) output: Option<i64>,
    pub(crate) package: Option<String>,
    pub(crate) api: Option<String>,
    /// Effort values the model takes, when the catalog lists them.
    pub(crate) efforts: Vec<String>,
    /// The field its reasoning is echoed back in (`interleaved`).
    pub(crate) reasoning_field: Option<String>,
    pub(crate) deprecated: bool,
    pub(crate) release_date: String,
}

impl Gateway {
    /// The gateway in a models.dev catalog (all of `api.json`, or the gateway's entry alone).
    fn parse(value: &Value, id: &str) -> Option<Self> {
        let provider = value.get(id)?.as_object()?;
        Some(Self {
            id: id.to_owned(),
            name: provider
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_owned(),
            package: provider
                .get("npm")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            api: provider
                .get("api")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            models: provider
                .get("models")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(id, model)| (id.clone(), parse_model(id, model)))
                .collect(),
        })
    }

    fn parse_go(value: &Value) -> Option<Self> {
        let models = value["data"].as_array()?;
        Some(Self {
            id: "opencode-go".to_owned(),
            name: "OpenCode Go".to_owned(),
            package: None,
            api: None,
            models: models
                .iter()
                .filter_map(|model| {
                    let id = model["id"].as_str()?;
                    Some((id.to_owned(), parse_go_model(id, model)))
                })
                .collect(),
        })
    }
}

/// The gateway's catalog, shared by every agent connection that keeps its copy at one place.
#[derive(Clone)]
pub(crate) struct Catalog {
    live: Arc<Live>,
}

struct Live {
    /// models.dev's `api.json`.
    url: String,
    go_url: String,
    /// Nori's copy of the gateway's entry.
    file: PathBuf,
    /// The catalog served, once there is one.
    gateway: tokio::sync::Mutex<Option<Arc<Vec<Gateway>>>>,
    /// When models.dev was last asked.
    asked: std::sync::Mutex<Option<Instant>>,
    /// How many times the catalog changed after it was first served.
    changed: tokio::sync::watch::Sender<i64>,
    periodic: std::sync::Once,
}

impl Catalog {
    /// The catalog of the environment `env`: one per copy on disk, for the whole process.
    pub(crate) fn shared(env: &Env) -> Self {
        static CATALOGS: std::sync::OnceLock<std::sync::Mutex<HashMap<PathBuf, Catalog>>> =
            std::sync::OnceLock::new();
        let file = crate::env::store_root(env).join("models.json");
        let url = format!(
            "{}/api.json",
            env.get("NORI_OPENCODE_MODELS_URL")
                .unwrap_or_else(|| DEFAULT_URL.to_owned())
                .trim_end_matches('/')
        );
        let go_url = env
            .get("NORI_OPENCODE_GO_MODELS_URL")
            .unwrap_or_else(|| GO_MODELS_URL.to_owned());
        let mut catalogs = CATALOGS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        catalogs
            .entry(file.clone())
            .or_insert_with(|| Self {
                live: Arc::new(Live {
                    url,
                    go_url,
                    file,
                    gateway: tokio::sync::Mutex::new(None),
                    asked: std::sync::Mutex::new(None),
                    changed: tokio::sync::watch::channel(0).0,
                    periodic: std::sync::Once::new(),
                }),
            })
            .clone()
    }

    /// Ticks each time the catalog changes under open sessions.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<i64> {
        self.live.changed.subscribe()
    }

    /// The catalog to serve now: in memory, else Nori's copy on disk, else models.dev's, else
    /// none. models.dev is asked again behind it.
    pub(crate) async fn models(&self) -> Arc<Vec<Gateway>> {
        let mut gateway = self.live.gateway.lock().await;
        let served = match gateway.as_ref() {
            Some(current) => Arc::clone(current),
            None => {
                let first = match read_copy(&self.live.file).await {
                    Some(copy) => copy,
                    None => match self.fetch().await {
                        Some(fetched) => {
                            self.keep(&fetched).await;
                            fetched
                        }
                        None => Value::Null,
                    },
                };
                let first = Arc::new(parse_catalogs(&first));
                *gateway = Some(Arc::clone(&first));
                first
            }
        };
        drop(gateway);
        self.refresh_soon();
        let catalog = self.clone();
        self.live.periodic.call_once(move || {
            tokio::spawn(async move {
                let mut ticks = tokio::time::interval(REFRESH_PERIOD);
                ticks.tick().await;
                loop {
                    ticks.tick().await;
                    catalog.refresh().await;
                }
            });
        });
        served
    }

    /// Ask models.dev again in the background, unless it was asked moments ago.
    fn refresh_soon(&self) {
        {
            let mut asked = self
                .live
                .asked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if asked.is_some_and(|at| at.elapsed() < REFRESH_EVERY) {
                return;
            }
            *asked = Some(Instant::now());
        }
        let catalog = self.clone();
        tokio::spawn(async move { catalog.refresh().await });
    }

    /// Ask models.dev, and serve what it says when that differs from what is served.
    async fn refresh(&self) {
        let Some(mut fetched) = self.fetch().await else {
            return;
        };
        if let Some(previous) = read_copy(&self.live.file).await {
            for id in [GATEWAY, "opencode-go"] {
                if fetched.get(id).is_none()
                    && let Some(catalog) = previous.get(id)
                {
                    fetched[id] = catalog.clone();
                }
            }
        }
        let parsed = parse_catalogs(&fetched);
        if parsed.is_empty() {
            return;
        }
        let mut gateway = self.live.gateway.lock().await;
        if gateway.as_deref() == Some(&parsed) {
            return;
        }
        self.keep(&fetched).await;
        let first = gateway.is_none();
        *gateway = Some(Arc::new(parsed));
        drop(gateway);
        if !first {
            self.live.changed.send_modify(|count| *count += 1);
        }
    }

    /// The gateway's entry of models.dev now, or `None` when it cannot be had.
    async fn fetch(&self) -> Option<Value> {
        let url = &self.live.url;
        let go_url = &self.live.go_url;
        let (models, go) = tokio::join!(fetch_json(url), fetch_json(go_url));
        let mut catalogs = serde_json::Map::new();
        if let Some(all) = models
            && let Some(gateway) = all.get(GATEWAY)
        {
            catalogs.insert(GATEWAY.to_owned(), gateway.clone());
        }
        if let Some(go) = go
            && Gateway::parse_go(&go).is_some()
        {
            catalogs.insert("opencode-go".to_owned(), go);
        }
        (!catalogs.is_empty()).then_some(Value::Object(catalogs))
    }

    /// Keep `entry` as Nori's copy.
    async fn keep(&self, entry: &Value) {
        let Ok(bytes) = serde_json::to_vec(entry) else {
            return;
        };
        let file = self.live.file.clone();
        let written = tokio::task::spawn_blocking(move || {
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let temporary = file.with_extension("json.tmp");
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, &file)
        })
        .await;
        if let Ok(Err(error)) = written {
            tracing::debug!(%error, "could not keep the OpenCode model catalog");
        }
    }
}

async fn fetch_json(url: &str) -> Option<Value> {
    let fetched: Result<Value, String> = async {
        let response = reqwest::Client::new()
            .get(url)
            .timeout(FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("{url}: {error}"))?
            .error_for_status()
            .map_err(|error| format!("{url}: {error}"))?;
        let value: Value = response
            .json()
            .await
            .map_err(|error| format!("{url}: {error}"))?;
        Ok(value)
    }
    .await;
    fetched
        .inspect_err(|error| tracing::debug!(%error, "could not fetch OpenCode model catalog"))
        .ok()
}

fn parse_catalogs(value: &Value) -> Vec<Gateway> {
    [
        Gateway::parse(value, GATEWAY),
        Gateway::parse_go(&value["opencode-go"]),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// Nori's copy at `file`, if it reads and names the gateway.
async fn read_copy(file: &std::path::Path) -> Option<Value> {
    let bytes = tokio::fs::read(file).await.ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    (value.get(GATEWAY).is_some() || value.get("opencode-go").is_some()).then_some(value)
}

fn parse_go_model(id: &str, model: &Value) -> CatalogModel {
    let api = &model["api"];
    let protocol = ["responses", "messages", "chat-completions"]
        .into_iter()
        .find(|protocol| api[*protocol].is_object());
    let endpoint = protocol.map(|protocol| &api[protocol]);
    let package = endpoint.and_then(|endpoint| endpoint["sdk"]["aisdk"].as_str());
    CatalogModel {
        id: model["id"].as_str().unwrap_or(id).to_owned(),
        name: model["name"].as_str().unwrap_or(id).to_owned(),
        reasoning: model["capabilities"]["reasoning"]["supported"]
            .as_bool()
            .unwrap_or(false),
        tool_call: model["capabilities"]["tools"]["supported"]
            .as_bool()
            .unwrap_or(true),
        images: model["modalities"]["input"]
            .as_array()
            .is_some_and(|input| {
                input
                    .iter()
                    .any(|kind| kind.as_str().is_some_and(|kind| kind.starts_with("image")))
            }),
        temperature: true,
        context: model["limit"]["context"].as_i64(),
        output: model["limit"]["output"].as_i64(),
        package: package.map(ToOwned::to_owned),
        api: endpoint
            .and_then(|endpoint| endpoint["base_url"].as_str())
            .map(ToOwned::to_owned),
        efforts: model["capabilities"]["reasoning"]["effort"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(ToOwned::to_owned)
            .collect(),
        reasoning_field: endpoint
            .and_then(|endpoint| endpoint["compatibility"]["reasoning_field"].as_str())
            .map(ToOwned::to_owned),
        deprecated: model["status"] == "deprecated",
        release_date: model["release_date"].as_str().unwrap_or("").to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::parse_catalogs;

    #[test]
    fn go_catalog_keeps_its_provider_and_model_api_metadata() {
        let catalogs = parse_catalogs(&json!({
            "opencode": {"name": "OpenCode Models", "models": {}},
            "opencode-go": {"data": [{
                "id": "deepseek-v4.1-flash",
                "name": "DeepSeek V4.1 Flash",
                "release_date": "2026-11-01",
                "limit": {"context": 1000000, "output": 64000},
                "capabilities": {
                    "tools": {"supported": true},
                    "reasoning": {"supported": true, "effort": ["low", "high"]}
                },
                "modalities": {"input": ["text", "image/jpeg"]},
                "api": {"responses": {
                    "base_url": "https://opencode.ai/inference/go/openai/v1",
                    "sdk": {"aisdk": "@ai-sdk/openai"},
                    "compatibility": {"reasoning_field": "reasoning_content"}
                }}
            }]}
        }));
        let go = catalogs
            .iter()
            .find(|catalog| catalog.id == "opencode-go")
            .unwrap();
        let model = &go.models["deepseek-v4.1-flash"];
        assert_eq!(go.name, "OpenCode Go");
        assert_eq!(
            model.api.as_deref(),
            Some("https://opencode.ai/inference/go/openai/v1")
        );
        assert_eq!(model.package.as_deref(), Some("@ai-sdk/openai"));
        assert_eq!(model.context, Some(1_000_000));
        assert_eq!(model.output, Some(64_000));
        assert_eq!(model.efforts, ["low", "high"]);
        assert_eq!(model.reasoning_field.as_deref(), Some("reasoning_content"));
        assert!(model.images);
    }

    #[test]
    fn missing_go_catalog_does_not_hide_open_code_models() {
        let catalogs = parse_catalogs(&json!({"opencode": {
            "name": "OpenCode Models", "models": {"model": {"id": "model"}}
        }}));
        assert_eq!(
            catalogs
                .iter()
                .map(|catalog| catalog.id.as_str())
                .collect::<Vec<_>>(),
            ["opencode"]
        );
    }
}

fn parse_model(id: &str, model: &Value) -> CatalogModel {
    let efforts = model["reasoning_options"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|option| option["type"] == "effort")
        .flat_map(|option| option["values"].as_array().cloned().unwrap_or_default())
        .filter_map(|value| value.as_str().map(ToOwned::to_owned))
        .filter(|value| value != "null")
        .collect();
    let reasoning_field = match &model["interleaved"] {
        Value::Bool(true) => Some("reasoning_content".to_owned()),
        Value::Object(interleaved) => interleaved
            .get("field")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    };
    CatalogModel {
        id: model["id"].as_str().unwrap_or(id).to_owned(),
        name: model["name"].as_str().unwrap_or(id).to_owned(),
        reasoning: model["reasoning"].as_bool().unwrap_or(false),
        tool_call: model["tool_call"].as_bool().unwrap_or(true),
        images: model["modalities"]["input"]
            .as_array()
            .map(|input| input.iter().any(|kind| kind == "image"))
            .unwrap_or_else(|| model["attachment"].as_bool().unwrap_or(false)),
        temperature: model["temperature"].as_bool().unwrap_or(true),
        context: model["limit"]["context"].as_i64(),
        output: model["limit"]["output"].as_i64(),
        package: model["provider"]["npm"].as_str().map(ToOwned::to_owned),
        api: model["provider"]["api"].as_str().map(ToOwned::to_owned),
        efforts,
        reasoning_field,
        deprecated: model["status"] == "deprecated",
        release_date: model["release_date"].as_str().unwrap_or("").to_owned(),
    }
}
