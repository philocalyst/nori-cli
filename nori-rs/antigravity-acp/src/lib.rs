//! The Antigravity SDK as an in-process ACP agent: an explicit alternative to the signed-in
//! official `agy` CLI, for API-key (`GEMINI_API_KEY`) and Vertex users.
//!
//! It drives a `localharness` through `antigravity-sdk-rust`. Select it with
//! `native = { agent = "antigravity-sdk" }`; the default `antigravity` agent is the official
//! `agy acp` process and needs none of this.

mod agent;
mod bridge;
mod connection;
mod discovery;
mod errors;
mod host;
mod lifecycle;
mod models;
mod prompt_codec;
mod session;
mod store;
mod translate;
mod turn;

use std::path::PathBuf;

use nori_acp_kit::Registration;
use nori_acp_kit::Sessions;
use nori_acp_kit::SpecOptions;

use crate::agent::AntigravityAgent;
use crate::discovery::Backend;
use crate::store::SessionStore;

/// What `native = { agent = "antigravity-sdk", config = [...] }` accepts.
const OPTIONS: &[&str] = &[
    "harness_path",
    "ws_url",
    "model",
    "thought_level",
    "effort",
    "save_dir",
];

/// This agent, for the application to hand to the host.
pub fn registration() -> Registration {
    Registration::new("antigravity-sdk", |spec| async move {
        let options = SpecOptions::parse("Antigravity SDK", &spec.config, OPTIONS)?;
        let thought_level = options
            .get_any(&["thought_level", "effort"])
            .map(str::to_owned);
        if let Some(level) = thought_level.as_deref()
            && !models::is_thought_level(level.trim())
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "unsupported Antigravity thought level {level:?}: use low, medium, high or max"
                ),
            ));
        }
        let save_dir = options
            .get("save_dir")
            .map(PathBuf::from)
            .or_else(|| spec.home.as_ref().map(|home| home.join("antigravity")));
        Ok(AntigravityAgent {
            backend: Backend {
                harness_path: options.get("harness_path").map(PathBuf::from).or_else(|| {
                    spec.environment
                        .get("ANTIGRAVITY_HARNESS_PATH")
                        .map(PathBuf::from)
                }),
                ws_url: options
                    .get("ws_url")
                    .map(str::to_owned)
                    .or_else(|| spec.environment.get("ANTIGRAVITY_WS_URL").cloned()),
                environment: spec.environment.clone(),
            },
            model: options
                .get("model")
                .map(str::to_owned)
                .or_else(|| spec.environment.get("ANTIGRAVITY_MODEL").cloned()),
            thought_level,
            sessions: Sessions::default(),
            store: SessionStore::new(save_dir.as_deref()),
        })
    })
}
