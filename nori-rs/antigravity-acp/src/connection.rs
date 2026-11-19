//! One session's connection to `localharness`, with reconnect on a live configuration change.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use antigravity_sdk_rust::connection::Connection as _;
use antigravity_sdk_rust::harness_config;
use antigravity_sdk_rust::hooks::HookRunner;
use antigravity_sdk_rust::local::LocalConnection;
use antigravity_sdk_rust::local::LocalConnectionStrategy;
use antigravity_sdk_rust::tools::ToolRunner;
use antigravity_sdk_rust::types::CapabilitiesConfig;
use antigravity_sdk_rust::types::Content;
use antigravity_sdk_rust::types::GeminiConfig;
use antigravity_sdk_rust::types::GenerationConfig;
use antigravity_sdk_rust::types::ModelConfig;
use antigravity_sdk_rust::types::ModelEntry;
use antigravity_sdk_rust::types::SessionContinuationMode;
use antigravity_sdk_rust::types::Step;
use antigravity_sdk_rust::types::ThinkingLevel;
use futures::stream::BoxStream;
use nori_acp_kit::AgentError;
use nori_acp_kit::HostBridge;
use nori_acp_kit::McpEndpoint;
use nori_acp_kit::Sink;
use tokio::sync::Mutex as AsyncMutex;

use crate::bridge;
use crate::bridge::AcpBridgeHook;
use crate::bridge::SinkSlot;
use crate::discovery::Backend;
use crate::discovery::HarnessEndpoint;
use crate::errors;
use crate::host;

/// Parameters for opening a session's connection.
#[derive(Clone)]
pub(crate) struct StartConfig {
    /// 32+ `[a-zA-Z0-9-]` characters, as the harness requires.
    pub(crate) session_id: String,
    pub(crate) cwd: PathBuf,
    pub(crate) additional_directories: Vec<PathBuf>,
    pub(crate) model: String,
    pub(crate) thinking_level: ThinkingLevel,
    pub(crate) host: Option<HostBridge>,
    pub(crate) mcp_servers: Vec<McpEndpoint>,
    pub(crate) save_dir: PathBuf,
}

/// What a fork starts from. The SDK cannot checkpoint a live trajectory, so a fork carries the
/// conversation as system context.
#[derive(Debug, Clone, Default)]
pub(crate) struct ForkSeed {
    pub(crate) source_session_id: String,
    pub(crate) history: Vec<Step>,
    pub(crate) source_save_dir: Option<PathBuf>,
}

/// A live `LocalConnection`. A change of model or thinking level reconnects on the next send,
/// because the harness reads both at connect time.
pub(crate) struct Connection {
    backend: Backend,
    config: AsyncMutex<StartConfig>,
    fork_seed: Option<ForkSeed>,
    sink: SinkSlot,
    live: Mutex<Arc<LocalConnection>>,
    needs_reconnect: AtomicBool,
    initial_history: Vec<Step>,
}

impl Connection {
    pub(crate) async fn open(
        config: StartConfig,
        backend: &Backend,
        fork_seed: Option<ForkSeed>,
    ) -> Result<Self, AgentError> {
        if let Some(source) = fork_seed
            .as_ref()
            .and_then(|seed| seed.source_save_dir.as_ref())
            && source != &config.save_dir
        {
            copy_state(source, &config.save_dir)?;
        }
        let sink: SinkSlot = Arc::new(RwLock::new(None));
        let live = Arc::new(connect(&config, backend, fork_seed.as_ref(), &sink).await?);
        Ok(Self {
            backend: backend.clone(),
            initial_history: live.initial_history().to_vec(),
            config: AsyncMutex::new(config),
            fork_seed,
            sink,
            live: Mutex::new(live),
            needs_reconnect: AtomicBool::new(false),
        })
    }

    fn current(&self) -> Arc<LocalConnection> {
        Arc::clone(&self.live.lock().unwrap_or_else(PoisonError::into_inner))
    }

    async fn ready(&self) -> Result<Arc<LocalConnection>, AgentError> {
        if !self.needs_reconnect.swap(false, Ordering::SeqCst) {
            return Ok(self.current());
        }
        let _ = self.current().disconnect().await;
        let config = self.config.lock().await.clone();
        let next =
            Arc::new(connect(&config, &self.backend, self.fork_seed.as_ref(), &self.sink).await?);
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) = Arc::clone(&next);
        Ok(next)
    }

    pub(crate) fn initial_history(&self) -> Vec<Step> {
        self.initial_history.clone()
    }

    /// Where the SDK's hooks send permission and question requests from now on.
    pub(crate) fn attach(&self, sink: Sink) {
        if let Ok(mut slot) = self.sink.write() {
            *slot = Some(sink);
        }
    }

    pub(crate) fn receive_steps(&self) -> BoxStream<'static, Result<Step, anyhow::Error>> {
        self.current().receive_steps()
    }

    pub(crate) async fn send(&self, content: &Content, model: &str) -> Result<(), AgentError> {
        let live = self.ready().await?;
        match content.as_text() {
            Some(text) => live.send(text).await,
            None => live.send_content(content).await,
        }
        .map_err(|error| errors::classify(&error, model))
    }

    pub(crate) async fn halt(&self) {
        let _ = self.current().send_halt_request().await;
    }

    pub(crate) async fn disconnect(&self) {
        let _ = self.current().disconnect().await;
    }

    pub(crate) async fn apply_config(&self, model: &str, thinking_level: ThinkingLevel) {
        let mut config = self.config.lock().await;
        if config.model != model || config.thinking_level != thinking_level {
            model.clone_into(&mut config.model);
            config.thinking_level = thinking_level;
            self.needs_reconnect.store(true, Ordering::SeqCst);
        }
    }
}

async fn connect(
    config: &StartConfig,
    backend: &Backend,
    fork_seed: Option<&ForkSeed>,
    sink: &SinkSlot,
) -> Result<LocalConnection, AgentError> {
    let endpoint = backend.resolve_endpoint()?;
    let vertex = harness_config::vertex_from_env()
        || ["GOOGLE_GENAI_USE_VERTEXAI", "GOOGLE_GENAI_USE_ENTERPRISE"]
            .iter()
            .filter_map(|key| backend.environment.get(*key))
            .any(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "true" | "1"));
    // Only whether a key is set matters here; its value is never read into a message.
    if !vertex && !backend.has_setting("GEMINI_API_KEY") {
        return Err(errors::missing_credentials());
    }

    let tool_runner = ToolRunner::new();
    if let Some(host) = &config.host {
        for tool in host::sdk_tools(host) {
            tool_runner
                .register(tool)
                .await
                .map_err(AgentError::internal)?;
        }
    }
    let hook_runner = HookRunner::new();
    hook_runner
        .register(Arc::new(AcpBridgeHook {
            sink: Arc::clone(sink),
            host: config.host.clone(),
        }))
        .await;

    let workspaces = std::iter::once(&config.cwd)
        .chain(&config.additional_directories)
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    let gemini_config = GeminiConfig {
        api_key: backend.environment.get("GEMINI_API_KEY").cloned(),
        vertex,
        project: backend
            .environment
            .get("GOOGLE_CLOUD_PROJECT")
            .cloned()
            .or_else(|| std::env::var("GOOGLE_CLOUD_PROJECT").ok()),
        location: backend
            .environment
            .get("GOOGLE_CLOUD_LOCATION")
            .cloned()
            .or_else(|| std::env::var("GOOGLE_CLOUD_LOCATION").ok()),
        models: ModelConfig {
            default: ModelEntry {
                name: config.model.clone(),
                api_key: None,
                generation: GenerationConfig {
                    thinking_level: Some(config.thinking_level),
                },
            },
            ..ModelConfig::default()
        },
        ..GeminiConfig::default()
    };

    let mut sections = host::instruction_sections(config.host.as_ref());
    sections.extend(bridge::fork_context(fork_seed));
    let binary_path = match &endpoint {
        HarnessEndpoint::Binary(path) => path.to_string_lossy().into_owned(),
        HarnessEndpoint::WsUrl(_) => String::new(),
    };
    let mut strategy = LocalConnectionStrategy::new(
        binary_path,
        gemini_config,
        CapabilitiesConfig::default(),
        host::appended(sections),
        Some(config.save_dir.to_string_lossy().into_owned()),
        workspaces,
        Vec::new(),
        Some(tool_runner),
        Some(hook_runner),
        config.session_id.clone(),
        Some(SessionContinuationMode::CreateOrResume),
        bridge::mcp_server_configs(&config.mcp_servers),
    );
    strategy.env = backend.environment.clone();

    match endpoint {
        HarnessEndpoint::Binary(_) => strategy.connect().await,
        HarnessEndpoint::WsUrl(url) => {
            let key = backend
                .environment
                .get("ANTIGRAVITY_HARNESS_API_KEY")
                .map(String::as_str);
            strategy.connect_ws(&url, key).await
        }
    }
    .map_err(|error| errors::classify(&error, &config.model))
}

/// Copy a session's harness state into a fork's directory, leaving out Nori's own metadata.
fn copy_state(source: &Path, dest: &Path) -> Result<(), AgentError> {
    if !source.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == crate::store::SESSION_META_FILE {
            continue;
        }
        let target = dest.join(name);
        if entry.file_type()?.is_dir() {
            copy_state(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}
