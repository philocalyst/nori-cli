//! Codex's model catalog, live: what the backend's `/models` last said for this account,
//! reached by a small endpoint client of Nori's.
//!
//! The catalog in memory is the one served. It starts as the account's last catalog on disk
//! (`models_cache.json`, shared with the Codex CLI), however old and whoever wrote it, and is
//! asked for again in the background whenever a session opens (at most every few minutes) and
//! periodically while Nori runs. A catalog that differs replaces it, is written back to disk, and
//! is announced through [`Catalog::changes`] so open sessions offer the new models at once. Only
//! an account with no catalog anywhere waits for the backend, and only an account that cannot
//! reach it falls back to the catalog this build shipped with.
//!
//! Each model offers exactly the reasoning efforts it lists (Codex's own `ultra` and
//! `persistent`, which drive Codex runtime features the engine replaces, are left out) and the
//! service tiers it advertises, plus `flex`.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_models_manager::cache::ModelsCacheEntry;
use codex_protocol::openai_models::InputModality;
use codex_protocol::openai_models::ModelInfo as CodexModel;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use nori_acp_kit::schema::SessionConfigOptionCategory;
use nori_engine::Dial;
use nori_engine::DialChoice;
use nori_engine::ModelInfo;
use serde_json::json;
use sha2::Digest;

/// How soon after one the opening of a session asks the backend again.
const REFRESH_EVERY: Duration = Duration::from_secs(300);
/// How often the backend is asked while Nori runs, sessions opening or not.
const REFRESH_PERIOD: Duration = Duration::from_secs(30 * 60);
/// How long a catalog request may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
/// The option id and request field of the service tier.
pub(crate) const SERVICE_TIER: &str = "service_tier";
/// The service tier that sends none: the account's standard routing.
const STANDARD_TIER: &str = "default";
const FLEX_TIER: &str = "flex";

/// Where the catalog comes from: the provider's address and headers.
#[derive(Clone)]
pub(crate) struct Source {
    pub(crate) provider_id: String,
    /// The provider's configured address, when it has one.
    pub(crate) base_url: Option<String>,
    /// Whether the provider takes Codex's own sign-in (OpenAI).
    pub(crate) openai_auth: bool,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) query: Vec<(String, String)>,
}

/// The catalog of one account. Cheap to clone: clones share one catalog.
#[derive(Clone)]
pub(crate) struct Catalog {
    live: Arc<Live>,
}

struct Live {
    endpoint: Endpoint,
    /// The account's `models_cache.json`.
    cache_file: PathBuf,
    /// The catalog served, once there is one.
    models: tokio::sync::Mutex<Option<Arc<Vec<CodexModel>>>>,
    /// When the backend was last asked.
    asked: std::sync::Mutex<Option<Instant>>,
    /// How many times the catalog changed after it was first served.
    changed: tokio::sync::watch::Sender<i64>,
    periodic: std::sync::Once,
}

/// What a workspace offers.
pub(crate) struct Offered {
    pub(crate) models: Vec<ModelInfo>,
    pub(crate) default_model: Option<String>,
    pub(crate) service_tier: Dial,
    /// Codex's own metadata of each offered model, by id (its slug).
    pub(crate) metadata: HashMap<String, Arc<CodexModel>>,
}

impl Catalog {
    pub(crate) fn new(home: PathBuf, source: Source, auth: Arc<AuthManager>) -> Self {
        Self {
            live: Arc::new(Live {
                endpoint: Endpoint {
                    source,
                    auth,
                    http: reqwest::Client::new(),
                },
                cache_file: home.join("models_cache.json"),
                models: tokio::sync::Mutex::new(None),
                asked: std::sync::Mutex::new(None),
                changed: tokio::sync::watch::channel(0).0,
                periodic: std::sync::Once::new(),
            }),
        }
    }

    /// Ticks each time the catalog changes under open sessions.
    pub(crate) fn changes(&self) -> tokio::sync::watch::Receiver<i64> {
        self.live.changed.subscribe()
    }

    /// The catalog to serve now: in memory, else the account's last one on disk, else the
    /// backend's, else the one this build shipped with. The backend is asked again behind it.
    async fn models(&self) -> Arc<Vec<CodexModel>> {
        let mut models = self.live.models.lock().await;
        let served = match models.as_ref() {
            Some(current) => Arc::clone(current),
            None => {
                let first = match read_cache(&self.live.cache_file).await {
                    Some(cached) => cached,
                    None => match self.live.endpoint.fetch().await {
                        Some(fetched) => {
                            write_cache(&self.live, &fetched).await;
                            fetched
                        }
                        None => codex_models_manager::bundled_models_response()
                            .map(|response| response.models)
                            .unwrap_or_default(),
                    },
                };
                let first = Arc::new(first);
                *models = Some(Arc::clone(&first));
                first
            }
        };
        drop(models);
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

    /// Ask the backend again in the background, unless it was asked moments ago.
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

    /// Ask the backend, and serve what it says when that differs from what is served.
    async fn refresh(&self) {
        let Some(fetched) = self.live.endpoint.fetch().await else {
            return;
        };
        let mut models = self.live.models.lock().await;
        if models.as_deref() == Some(&fetched) {
            return;
        }
        write_cache(&self.live, &fetched).await;
        let first = models.is_none();
        *models = Some(Arc::new(fetched));
        drop(models);
        if !first {
            self.live.changed.send_modify(|count| *count += 1);
        }
    }

    /// The models offered now. `model`, `effort` and `tier` are what the configuration asks for.
    pub(crate) async fn offered(
        &self,
        model: Option<&str>,
        effort: Option<&str>,
        tier: Option<&str>,
    ) -> Offered {
        let known = self.models().await;
        let mut presets: Vec<ModelPreset> = known.iter().cloned().map(ModelPreset::from).collect();
        presets.sort_by_key(|preset| {
            known
                .iter()
                .find(|info| info.slug == preset.model)
                .map_or(i32::MAX, |info| info.priority)
        });
        let codex_backend = self.live.endpoint.auth.current_auth_uses_codex_backend();
        let mut presets = ModelPreset::filter_by_auth(presets, codex_backend);
        ModelPreset::mark_default_by_picker_visibility(&mut presets);
        let mut metadata = HashMap::new();
        let mut models = Vec::new();
        for preset in presets
            .iter()
            .filter(|preset| preset.show_in_picker || Some(preset.model.as_str()) == model)
        {
            let Some(info) = known.iter().find(|info| info.slug == preset.model).cloned() else {
                continue;
            };
            models.push(ModelInfo {
                context_window: info.usable_context_window(),
                ..engine_model(preset, effort)
            });
            metadata.insert(preset.model.clone(), Arc::new(info));
        }
        if let Some(model) = model.filter(|model| !metadata.contains_key(*model)) {
            // A model the catalog does not list still runs: as the listed model its name extends
            // (a dated snapshot of it), else with Codex's generic metadata.
            let info = known
                .iter()
                .filter(|info| model.starts_with(info.slug.as_str()))
                .max_by_key(|info| info.slug.len())
                .map_or_else(
                    || codex_models_manager::model_info::model_info_from_slug(model),
                    |info| CodexModel {
                        slug: model.to_owned(),
                        ..info.clone()
                    },
                );
            let preset = ModelPreset::from(info.clone());
            models.push(ModelInfo {
                context_window: info.usable_context_window(),
                ..engine_model(&preset, effort)
            });
            metadata.insert(model.to_owned(), Arc::new(info));
        }
        let default_model = model
            .map(ToOwned::to_owned)
            .or_else(|| {
                presets
                    .iter()
                    .find(|preset| preset.is_default)
                    .map(|preset| preset.model.clone())
            })
            .or_else(|| models.first().map(|model| model.id.clone()));
        Offered {
            service_tier: service_tier_dial(&presets, &models, tier),
            models,
            default_model,
            metadata,
        }
    }
}

/// The models of the catalog cached at `file`, however old, if it reads and lists any.
async fn read_cache(file: &std::path::Path) -> Option<Vec<CodexModel>> {
    let bytes = tokio::fs::read(file).await.ok()?;
    let entry: ModelsCacheEntry = serde_json::from_slice(&bytes).ok()?;
    (!entry.models.is_empty()).then_some(entry.models)
}

/// Keep `models` as the account's catalog on disk, in the form the Codex CLI reads too.
async fn write_cache(live: &Live, models: &[CodexModel]) {
    let auth = live.endpoint.auth.auth().await;
    let entry = ModelsCacheEntry {
        fetched_at: chrono::Utc::now(),
        etag: None,
        client_version: Some(codex_models_manager::client_version_to_whole()),
        identity: Some(live.endpoint.identity_of(auth.as_ref())),
        models: models.to_vec(),
    };
    let Ok(bytes) = serde_json::to_vec_pretty(&entry) else {
        return;
    };
    let file = live.cache_file.clone();
    let written = tokio::task::spawn_blocking(move || {
        let temporary = file.with_extension("json.nori-tmp");
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &file)
    })
    .await;
    if let Ok(Err(error)) = written {
        tracing::debug!(%error, "could not keep the model catalog");
    }
}

/// A Codex preset as an engine model: its own efforts (the configured one as its default when it
/// takes it) and whether it reads images.
fn engine_model(preset: &ModelPreset, effort: Option<&str>) -> ModelInfo {
    let efforts: Vec<String> = preset
        .supported_reasoning_efforts
        .iter()
        .filter(|level| {
            !matches!(
                level.effort,
                ReasoningEffort::Ultra | ReasoningEffort::Persistent
            )
        })
        .map(|level| level.effort.as_str().to_owned())
        .collect();
    let default_effort = effort
        .filter(|effort| efforts.iter().any(|known| known == effort))
        .map(ToOwned::to_owned)
        .or_else(|| Some(preset.default_reasoning_effort.as_str().to_owned()))
        .filter(|effort| efforts.contains(effort));
    ModelInfo {
        id: preset.model.clone(),
        label: preset.display_name.clone(),
        description: Some(preset.description.clone()).filter(|text| !text.is_empty()),
        context_window: None,
        max_output_tokens: None,
        efforts,
        default_effort,
        images: preset.input_modalities.contains(&InputModality::Image),
    }
}

/// The service tier: standard routing, then the tiers the models advertise (each offered by the
/// models that advertise it), then flex, which the API takes for any model.
fn service_tier_dial(
    presets: &[ModelPreset],
    models: &[ModelInfo],
    configured: Option<&str>,
) -> Dial {
    let mut choices = vec![DialChoice {
        value: STANDARD_TIER.to_owned(),
        label: "Standard".to_owned(),
        description: Some("Standard request routing".to_owned()),
        body: None,
        models: Vec::new(),
    }];
    for preset in presets
        .iter()
        .filter(|preset| models.iter().any(|model| model.id == preset.model))
    {
        for tier in &preset.service_tiers {
            match choices.iter_mut().find(|choice| choice.value == tier.id) {
                Some(choice) => choice.models.push(preset.model.clone()),
                None => choices.push(DialChoice {
                    value: tier.id.clone(),
                    label: tier.name.clone(),
                    description: Some(tier.description.clone()).filter(|text| !text.is_empty()),
                    body: Some(json!(tier.id)),
                    models: vec![preset.model.clone()],
                }),
            }
        }
    }
    match choices.iter_mut().find(|choice| choice.value == FLEX_TIER) {
        Some(flex) => flex.models.clear(),
        None => choices.push(DialChoice {
            value: FLEX_TIER.to_owned(),
            label: "Flex".to_owned(),
            description: Some("Lower-cost processing that may be slower".to_owned()),
            body: Some(json!(FLEX_TIER)),
            models: Vec::new(),
        }),
    }
    let default = configured
        .filter(|tier| choices.iter().any(|choice| choice.value == *tier))
        .unwrap_or(STANDARD_TIER)
        .to_owned();
    Dial {
        id: SERVICE_TIER.to_owned(),
        label: "Service tier".to_owned(),
        description: Some("Choose the request priority tier used for model calls".to_owned()),
        category: SessionConfigOptionCategory::ModelConfig,
        field: SERVICE_TIER.to_owned(),
        choices,
        default,
    }
}

/// The backend's `/models`, with the account's credential.
struct Endpoint {
    source: Source,
    auth: Arc<AuthManager>,
    http: reqwest::Client,
}

impl fmt::Debug for Endpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Endpoint")
            .field("provider", &self.source.provider_id)
            .finish_non_exhaustive()
    }
}

impl Endpoint {
    fn base_url(&self, auth: Option<&CodexAuth>) -> String {
        match &self.source.base_url {
            Some(url) => url.clone(),
            // Codex metadata is served by the Codex backend, for an API key too.
            None if self.source.openai_auth => CHATGPT_CODEX_BASE_URL.to_owned(),
            None if auth.is_some_and(CodexAuth::uses_codex_backend) => {
                CHATGPT_CODEX_BASE_URL.to_owned()
            }
            None => "https://api.openai.com/v1".to_owned(),
        }
    }

    /// The catalog the backend has for this account now, or `None` when it cannot be had.
    async fn fetch(&self) -> Option<Vec<CodexModel>> {
        let fetched: Result<Vec<CodexModel>, String> = async {
            let client_version = codex_models_manager::client_version_to_whole();
            let auth = if self.source.openai_auth {
                self.auth.auth().await
            } else {
                None
            };
            let url = format!(
                "{}/models",
                self.base_url(auth.as_ref()).trim_end_matches('/')
            );
            let mut request = self
                .http
                .get(&url)
                .query(&[("client_version", client_version.as_str())])
                .query(&self.source.query)
                .header(
                    "originator",
                    codex_login::default_client::originator().value,
                )
                .header(
                    "user-agent",
                    codex_login::default_client::get_codex_user_agent(),
                );
            for (name, value) in &self.source.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            if let Some(auth) = &auth {
                if let Ok(token) = auth.get_token() {
                    request = request.bearer_auth(token);
                }
                if let Some(account) = auth.get_account_id() {
                    request = request.header("chatgpt-account-id", account);
                }
            }
            let response = tokio::time::timeout(FETCH_TIMEOUT, request.send())
                .await
                .map_err(|_| format!("{url} did not answer in time"))?
                .map_err(|error| format!("{url}: {error}"))?;
            if !response.status().is_success() {
                return Err(format!("{url} answered {}", response.status()));
            }
            let body = response
                .bytes()
                .await
                .map_err(|error| format!("{url}: {error}"))?;
            let catalog: ModelsResponse =
                serde_json::from_slice(&body).map_err(|error| format!("{url}: {error}"))?;
            Ok(catalog.models)
        }
        .await;
        fetched
            .inspect_err(|error| {
                tracing::debug!(%error, provider = %self.source.provider_id, "no model catalog");
            })
            .ok()
    }

    /// The provider and principal a catalog belongs to, as a digest (no credential in it).
    fn identity_of(&self, auth: Option<&CodexAuth>) -> String {
        let mut digest = sha2::Sha256::new();
        for field in [
            "nori-codex-models-v1".to_owned(),
            self.source.provider_id.clone(),
            self.base_url(auth),
            format!("{:?}", auth.map(CodexAuth::api_auth_mode)),
            format!("{:?}", auth.and_then(CodexAuth::get_account_id)),
            format!("{:?}", auth.and_then(CodexAuth::get_chatgpt_user_id)),
            format!("{:?}", auth.and_then(CodexAuth::account_plan_type)),
        ] {
            digest.update(i64::try_from(field.len()).unwrap_or(i64::MAX).to_le_bytes());
            digest.update(field.as_bytes());
        }
        if let Some(key) = auth.and_then(CodexAuth::api_key) {
            digest.update(sha2::Sha256::digest(key.as_bytes()));
        }
        format!("{:x}", digest.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn the_bundled_catalog_offers_each_model_its_own_efforts_and_tiers() {
        let presets: Vec<ModelPreset> = codex_models_manager::bundled_models_response()
            .unwrap_or_else(|error| panic!("{error}"))
            .models
            .into_iter()
            .map(ModelPreset::from)
            .collect();
        let models: Vec<ModelInfo> = presets
            .iter()
            .map(|preset| engine_model(preset, Some("xhigh")))
            .collect();
        for (model, preset) in models.iter().zip(&presets) {
            assert!(
                !model.efforts.iter().any(|effort| effort == "ultra"),
                "{}",
                model.id
            );
            let listed = preset
                .supported_reasoning_efforts
                .iter()
                .any(|level| level.effort.as_str() == "xhigh");
            assert_eq!(
                model.default_effort.as_deref() == Some("xhigh"),
                listed,
                "{}",
                model.id
            );
        }
        let dial = service_tier_dial(&presets, &models, Some("priority"));
        assert_eq!(dial.default, "priority");
        let values: Vec<&str> = dial
            .choices
            .iter()
            .map(|choice| choice.value.as_str())
            .collect();
        assert_eq!(
            (values.first(), values.last()),
            (Some(&"default"), Some(&"flex"))
        );
    }

    /// A model released after this build shipped is offered from the account's cache however
    /// old it is and whichever client wrote it.
    #[tokio::test]
    async fn the_accounts_cached_catalog_is_offered_however_old() {
        let home = tempfile::tempdir().unwrap_or_else(|error| panic!("{error}"));
        let mut models = codex_models_manager::bundled_models_response()
            .unwrap_or_else(|error| panic!("{error}"))
            .models;
        let mut newest = models[0].clone();
        newest.slug = "gpt-from-the-future".to_owned();
        newest.display_name = "GPT from the future".to_owned();
        newest.priority = -1;
        models.push(newest);
        let entry = codex_models_manager::cache::ModelsCacheEntry {
            fetched_at: chrono::DateTime::UNIX_EPOCH,
            etag: None,
            client_version: Some("0.0.1".to_owned()),
            identity: Some("another client".to_owned()),
            models,
        };
        std::fs::write(
            home.path().join("models_cache.json"),
            serde_json::to_vec(&entry).unwrap_or_else(|error| panic!("{error}")),
        )
        .unwrap_or_else(|error| panic!("{error}"));
        let catalog = Catalog::new(
            home.path().to_path_buf(),
            Source {
                provider_id: "openai".to_owned(),
                base_url: Some("http://127.0.0.1:9".to_owned()),
                openai_auth: true,
                headers: Vec::new(),
                query: Vec::new(),
            },
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
        );
        let offered = catalog.offered(None, None, None).await;
        assert_eq!(
            (
                offered.default_model.as_deref(),
                offered.models.first().map(|model| model.id.as_str())
            ),
            (Some("gpt-from-the-future"), Some("gpt-from-the-future"))
        );
    }
}
