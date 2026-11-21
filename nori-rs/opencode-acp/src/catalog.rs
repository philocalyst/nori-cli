//! The gateway's models, live, from models.dev (its `opencode` provider: OpenCode Zen).
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
/// How soon after one the opening of a session asks models.dev again.
const REFRESH_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often models.dev is asked while Nori runs, sessions opening or not.
const REFRESH_PERIOD: Duration = Duration::from_secs(30 * 60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The gateway as models.dev describes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Gateway {
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
    fn parse(value: &Value) -> Option<Self> {
        let provider = value.get(GATEWAY)?.as_object()?;
        Some(Self {
            name: provider
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("OpenCode")
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
}

/// The gateway's catalog, shared by every agent connection that keeps its copy at one place.
#[derive(Clone)]
pub(crate) struct Catalog {
    live: Arc<Live>,
}

struct Live {
    /// models.dev's `api.json`.
    url: String,
    /// Nori's copy of the gateway's entry.
    file: PathBuf,
    /// The catalog served, once there is one.
    gateway: tokio::sync::Mutex<Option<Arc<Gateway>>>,
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
        let mut catalogs = CATALOGS
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        catalogs
            .entry(file.clone())
            .or_insert_with(|| Self {
                live: Arc::new(Live {
                    url,
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
    pub(crate) async fn models(&self) -> Arc<Gateway> {
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
                let first = Arc::new(Gateway::parse(&first).unwrap_or_default());
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
        let Some(fetched) = self.fetch().await else {
            return;
        };
        let Some(parsed) = Gateway::parse(&fetched) else {
            return;
        };
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
        let fetched: Result<Value, String> = async {
            let response = reqwest::Client::new()
                .get(url)
                .timeout(FETCH_TIMEOUT)
                .send()
                .await
                .map_err(|error| format!("{url}: {error}"))?
                .error_for_status()
                .map_err(|error| format!("{url}: {error}"))?;
            let mut all: Value = response
                .json()
                .await
                .map_err(|error| format!("{url}: {error}"))?;
            let gateway = all
                .get_mut(GATEWAY)
                .map(Value::take)
                .ok_or_else(|| format!("{url} lists no `{GATEWAY}` provider"))?;
            Ok(serde_json::json!({ GATEWAY: gateway }))
        }
        .await;
        fetched
            .inspect_err(|error| tracing::debug!(%error, "no OpenCode model catalog"))
            .ok()
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

/// Nori's copy at `file`, if it reads and names the gateway.
async fn read_copy(file: &std::path::Path) -> Option<Value> {
    let bytes = tokio::fs::read(file).await.ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get(GATEWAY).is_some().then_some(value)
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
