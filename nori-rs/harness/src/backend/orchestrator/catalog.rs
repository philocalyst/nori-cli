//! The models and settings each agent offers, remembered across runs, so `agent_capabilities`
//! lists them for agents that have not run in this process.
//!
//! Every session that opens records what its agent advertised. The catalog is kept in memory
//! and in `<nori home>/agent-models.json`, which is read once per process and rewritten in the
//! background, never on the way to a frame.

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::sync::PoisonError;

use nori_protocol::acp::v1 as acp;

use super::models::Model;
use super::models::Models;
use crate::recovery::Availability;

const FILE: &str = "agent-models.json";

/// What one agent offered when it last ran.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Offer {
    pub(super) models: Vec<Model>,
    /// Its other select settings, such as reasoning effort or mode.
    #[serde(default)]
    pub(super) settings: Vec<Setting>,
    /// When it was last read, in seconds since the Unix epoch.
    #[serde(default)]
    pub(super) refreshed_at: i64,
    /// Models a session found it cannot run, though the agent still lists them.
    #[serde(default)]
    pub(super) unavailable: Vec<Unavailable>,
}

/// A model that failed as unavailable (HTTP 404, a stale id), and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Unavailable {
    pub(super) model: String,
    pub(super) reason: String,
    /// When it failed, in seconds since the Unix epoch.
    pub(super) at: i64,
}

/// How long a failed model stays marked before it is offered again.
const UNAVAILABLE_FOR_SECONDS: i64 = 6 * 60 * 60;

/// A model an agent last advertised, with whether it can be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfferedModel {
    pub(crate) value: String,
    pub(crate) name: String,
    pub(crate) availability: Availability,
}

/// The models `agent` last advertised. A placeholder id is never runnable, and a model that
/// failed as unavailable recently is reported with that failure instead of as runnable.
pub(crate) fn offered_models(home: &Path, agent: &str) -> Vec<OfferedModel> {
    let offer = remembered(home, agent);
    let now = now_seconds();
    offer
        .models
        .into_iter()
        .map(|model| {
            let failed = offer
                .unavailable
                .iter()
                .find(|mark| mark.model == model.value && now - mark.at < UNAVAILABLE_FOR_SECONDS);
            let availability = if model
                .value
                .to_ascii_uppercase()
                .contains("MODEL_PLACEHOLDER")
            {
                Availability::Unavailable {
                    reason: "the agent advertised an unfilled model placeholder".to_owned(),
                }
            } else if let Some(failed) = failed {
                Availability::Unavailable {
                    reason: failed.reason.clone(),
                }
            } else {
                Availability::Runnable
            };
            OfferedModel {
                value: model.value,
                name: model.name,
                availability,
            }
        })
        .collect()
}

/// Whether `agent` advertises `model` as of when it last ran (a placeholder id never counts).
pub(crate) fn advertises(home: &Path, agent: &str, model: &str) -> bool {
    !model.to_ascii_uppercase().contains("MODEL_PLACEHOLDER")
        && remembered(home, agent)
            .models
            .iter()
            .any(|offered| offered.value == model)
}

/// `model` is confirmed to work on `agent` again: it is offered as runnable once more.
pub(crate) fn clear_unavailable(home: &Path, agent: &str, model: &str) {
    let mut offer = remembered(home, agent);
    let before = offer.unavailable.len();
    offer.unavailable.retain(|mark| mark.model != model);
    if offer.unavailable.len() != before {
        store(home, agent, offer);
    }
}

/// Remember that `model` failed on `agent` with `reason`, so nothing offers it as runnable
/// until the mark expires.
pub(crate) fn mark_unavailable(home: &Path, agent: &str, model: &str, reason: &str) {
    let mut offer = remembered(home, agent);
    offer.unavailable.retain(|mark| mark.model != model);
    offer.unavailable.push(Unavailable {
        model: model.to_owned(),
        reason: reason.to_owned(),
        at: now_seconds(),
    });
    store(home, agent, offer);
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct Setting {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) values: Vec<Model>,
}

#[derive(Default)]
struct Catalog {
    homes: BTreeMap<PathBuf, HomeCatalog>,
}

struct HomeCatalog {
    agents: BTreeMap<String, Offer>,
    dirty: BTreeMap<String, Offer>,
    generation: i64,
    writer: std::sync::Arc<Mutex<()>>,
}

static CATALOG: LazyLock<Mutex<Catalog>> = LazyLock::new(Default::default);

fn with_catalog<T>(home: &Path, read: impl FnOnce(&mut HomeCatalog) -> T) -> T {
    let mut catalog = CATALOG.lock().unwrap_or_else(PoisonError::into_inner);
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let entry = catalog
        .homes
        .entry(home.clone())
        .or_insert_with(|| HomeCatalog {
            agents: std::fs::read(home.join(FILE))
                .ok()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                .unwrap_or_default(),
            dirty: BTreeMap::new(),
            generation: 0,
            writer: std::sync::Arc::new(Mutex::new(())),
        });
    read(entry)
}

/// Remember what `agent` offers, from a session's advertised options.
pub(crate) fn remember(home: &Path, agent: &str, options: &[acp::SessionConfigOption]) {
    let offer = Offer {
        models: Models::of(options)
            .map(|models| models.offered)
            .unwrap_or_default(),
        settings: Models::settings(options)
            .into_iter()
            .map(|setting| Setting {
                id: setting.config_id,
                name: setting.name,
                values: setting.offered,
            })
            .collect(),
        refreshed_at: now_seconds(),
        // A fresh advertisement does not prove a failed model works; keep its marks while
        // the agent still lists it.
        unavailable: remembered(home, agent)
            .unavailable
            .into_iter()
            .filter(|mark| options_list_model(options, &mark.model))
            .collect(),
    };
    if offer.models.is_empty() && offer.settings.is_empty() {
        return;
    }
    store(home, agent, offer);
}

fn options_list_model(options: &[acp::SessionConfigOption], model: &str) -> bool {
    Models::of(options).is_some_and(|models| models.offered.iter().any(|m| m.value == model))
}

/// Keep `offer` as what `agent` offers: in memory now, on disk in the background.
fn store(home: &Path, agent: &str, offer: Offer) {
    let home = home.canonicalize().unwrap_or_else(|_| home.to_owned());
    let (generation, writer) = with_catalog(&home, |catalog| {
        catalog.agents.insert(agent.to_owned(), offer.clone());
        catalog.dirty.insert(agent.to_owned(), offer);
        catalog.generation = catalog.generation.saturating_add(1);
        (catalog.generation, std::sync::Arc::clone(&catalog.writer))
    });
    let write = move || {
        let _guard = writer.lock().unwrap_or_else(PoisonError::into_inner);
        let dirty = {
            let catalog = CATALOG.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(current) = catalog.homes.get(&home) else {
                return;
            };
            if current.generation != generation {
                return;
            }
            current.dirty.clone()
        };
        // Other Nori processes share this file. Merge only offers changed by
        // this process, under an OS lock, so their providers cannot disappear
        // when a stale in-memory snapshot finishes writing later.
        let Ok(lock) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(home.join("agent-models.lock"))
        else {
            return;
        };
        if lock.lock().is_err() {
            return;
        }
        let path = home.join(FILE);
        let mut agents: BTreeMap<String, Offer> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        agents.extend(dirty.clone());
        let Ok(json) = serde_json::to_vec_pretty(&agents) else {
            return;
        };
        let process = std::process::id();
        let temporary = home.join(format!("{FILE}.{process}.{generation}.tmp"));
        if std::fs::write(&temporary, json).is_ok() && std::fs::rename(&temporary, &path).is_ok() {
            let mut catalog = CATALOG.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(current) = catalog.homes.get_mut(&home) {
                for (agent, offer) in dirty {
                    if current.dirty.get(&agent) == Some(&offer) {
                        current.dirty.remove(&agent);
                    }
                }
                agents.extend(current.dirty.clone());
                current.agents = agents;
            }
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            runtime.spawn_blocking(write);
        }
        Err(_) => write(),
    }
}

pub(super) fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64)
}

/// What `agent` offered when it last ran.
pub(super) fn remembered(home: &Path, agent: &str) -> Offer {
    with_catalog(home, |catalog| {
        catalog.agents.get(agent).cloned().unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[tokio::test]
    async fn models_are_kept_across_runs() {
        let home = tempfile::tempdir().expect("home");
        let options = vec![
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "gpt-5.1".to_owned(),
                vec![acp::SessionConfigSelectOption::new("gpt-5.1", "GPT-5.1")],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ];
        remember(home.path(), "codex", &options);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !home.path().join(FILE).exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // A new process reads the file.
        CATALOG
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .homes
            .remove(&home.path().canonicalize().expect("canonical home"));
        assert_eq!(
            remembered(home.path(), "codex").models,
            vec![Model {
                value: "gpt-5.1".to_owned(),
                name: "GPT-5.1".to_owned(),
            }]
        );
        assert!(remembered(home.path(), "codex").refreshed_at > 0);
        assert_eq!(remembered(home.path(), "claude"), Offer::default());
    }

    #[tokio::test]
    async fn independent_homes_keep_their_catalogs_when_writes_overlap() {
        let first = tempfile::tempdir().expect("first home");
        let second = tempfile::tempdir().expect("second home");
        let options = |value: &str| {
            vec![
                acp::SessionConfigOption::select(
                    "model",
                    "Model",
                    value.to_owned(),
                    vec![acp::SessionConfigSelectOption::new(
                        value.to_owned(),
                        value.to_owned(),
                    )],
                )
                .category(acp::SessionConfigOptionCategory::Model),
            ]
        };
        let writer = with_catalog(first.path(), |catalog| {
            std::sync::Arc::clone(&catalog.writer)
        });
        {
            let _guard = writer.lock().unwrap_or_else(PoisonError::into_inner);
            remember(first.path(), "codex", &options("first"));
            remember(second.path(), "opencode", &options("second"));
            assert!(!first.path().join(FILE).exists());
            // The first write is blocked: switching homes cannot recover its
            // offers from disk, and must retain them in memory.
            assert_eq!(remembered(first.path(), "codex").models[0].value, "first");
            remember(first.path(), "codex", &options("latest"));
            remember(first.path(), "claude", &options("another agent"));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (!first.path().join(FILE).exists() || !second.path().join(FILE).exists())
            && std::time::Instant::now() < deadline
        {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let persisted: BTreeMap<String, Offer> = serde_json::from_slice(
            &std::fs::read(first.path().join(FILE)).expect("persisted first home"),
        )
        .expect("catalog JSON");
        assert_eq!(
            persisted.keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["claude", "codex"]
        );
        assert_eq!(persisted["codex"].models[0].value, "latest");
        assert_eq!(remembered(first.path(), "codex").models[0].value, "latest");
        assert_eq!(
            remembered(second.path(), "opencode").models[0].value,
            "second"
        );
    }

    #[tokio::test]
    async fn persistence_merges_external_offers_instead_of_overwriting_with_a_stale_snapshot() {
        let home = tempfile::tempdir().expect("home");
        assert_eq!(remembered(home.path(), "external"), Offer::default());
        let external = Offer {
            models: vec![Model {
                value: "external-model".to_owned(),
                name: "external-model".to_owned(),
            }],
            ..Offer::default()
        };
        let path = home.path().join(FILE);
        std::fs::write(
            &path,
            serde_json::to_vec(&BTreeMap::from([("external".to_owned(), external.clone())]))
                .expect("external catalog JSON"),
        )
        .expect("another process's catalog write");
        let options = vec![
            acp::SessionConfigOption::select(
                "model",
                "Model",
                "local-model",
                vec![acp::SessionConfigSelectOption::new("local-model", "Local")],
            )
            .category(acp::SessionConfigOptionCategory::Model),
        ];
        remember(home.path(), "local", &options);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let persisted: BTreeMap<String, Offer> =
                serde_json::from_slice(&std::fs::read(&path).expect("catalog file"))
                    .expect("catalog JSON");
            if persisted.contains_key("local") {
                assert_eq!(persisted.get("external"), Some(&external));
                assert_eq!(persisted.len(), 2);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "catalog did not persist"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
