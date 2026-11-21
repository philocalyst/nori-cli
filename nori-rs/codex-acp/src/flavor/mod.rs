//! Codex as an engine flavor: Codex's sign-in, configuration, catalog, prompts and tools, with
//! Nori's engine (`nori-engine`) running the loop against the Responses API directly.
//!
//! - `env`: the account's variables and directories.
//! - `config`: `config.toml`, layered as Codex layers it.
//! - `auth`: Codex's `AuthManager` over the account's `CODEX_HOME`.
//! - `catalog`: the account's live model catalog; efforts and service tiers per model.
//! - `instructions`: `AGENTS.md` as Codex finds it.
//! - `modes`: Codex's approval presets as rules, and its slash commands.
//! - `limits`: the plan's usage windows from response headers.

mod auth;
mod catalog;
mod config;
mod env;
mod instructions;
mod limits;
mod modes;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use codex_model_provider_info::CHATGPT_CODEX_BASE_URL;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::openai_models::ModelInfo as CodexModel;
use nori_acp_kit::AgentError;
use nori_acp_kit::AuthStatus;
use nori_acp_kit::InProcessSpec;
use nori_acp_kit::RateLimitWindow;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::AuthMethod;
use nori_engine::Connection;
use nori_engine::EffortStyle;
use nori_engine::Flavor;
use nori_engine::Identity;
use nori_engine::ModelInfo;
use nori_engine::Workspace;
use nori_engine::tools;
use nori_llm::Auth;
use nori_llm::Endpoint;
use nori_llm::Secret;
use nori_llm::Wire;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use auth::Account;
use catalog::Catalog;
use catalog::Source;
use config::CodexConfig;
use env::Env;

const OPENAI_API_BASE_URL: &str = "https://api.openai.com/v1";
const UPDATE_PLAN_DESCRIPTION: &str = "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n";

pub(crate) struct CodexFlavor {
    env: Env,
    /// The account's `CODEX_HOME`.
    home: PathBuf,
    /// `codex -c` overrides from the account.
    overrides: Vec<(String, toml::Value)>,
    store_root: PathBuf,
    account: Account,
    catalog: Catalog,
    /// `model_instructions_file`: base instructions that replace every model's own.
    base_instructions: Option<String>,
    /// Each working directory's configuration, as its last session opened it.
    configs: Mutex<HashMap<PathBuf, Arc<CodexConfig>>>,
    /// Codex's metadata of every model offered so far, by id.
    models: Mutex<HashMap<String, Arc<CodexModel>>>,
}

/// A provider's own headers: `http_headers` and the `env_http_headers` that are set.
fn provider_headers(provider: &ModelProviderInfo, env: &Env) -> Vec<(String, String)> {
    let mut headers: Vec<(String, String)> = provider
        .http_headers
        .iter()
        .flatten()
        .map(|(name, value)| (name.clone(), value.as_str().to_owned()))
        .collect();
    headers.extend(
        provider
            .env_http_headers
            .iter()
            .flatten()
            .filter_map(|(name, variable)| Some((name.clone(), env.get(variable)?))),
    );
    headers
}

fn provider_query(provider: &ModelProviderInfo) -> Vec<(String, String)> {
    provider
        .query_params
        .iter()
        .flatten()
        .map(|(name, value)| (name.clone(), value.as_str().to_owned()))
        .collect()
}

impl CodexFlavor {
    /// The agent an in-process spec describes: the account's home (else `CODEX_HOME`, else
    /// `~/.codex`), its `codex -c` overrides and its variables.
    pub(crate) async fn launch(spec: InProcessSpec) -> std::io::Result<Self> {
        let env = Env::inherited(spec.environment.clone());
        let home = spec.home.clone().unwrap_or_else(|| env.codex_home());
        let overrides = config::parse_overrides(&spec.config)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
        Self::open(env, home, overrides).await
    }

    async fn open(
        env: Env,
        home: PathBuf,
        overrides: Vec<(String, toml::Value)>,
    ) -> std::io::Result<Self> {
        let (read_home, read_overrides) = (home.clone(), overrides.clone());
        let launch = tokio::task::spawn_blocking(move || {
            let config = CodexConfig::load(&read_home, None, &read_overrides);
            let instructions = config
                .instructions_file(&read_home)
                .and_then(|file| std::fs::read_to_string(file).ok());
            (config, instructions)
        })
        .await
        .map_err(std::io::Error::other)?;
        let (config, base_instructions) = launch;
        let account = Account::open(home.clone(), &config, env.clone()).await?;
        let provider = config
            .providers()
            .ok()
            .and_then(|mut providers| providers.remove(&config.provider_id()))
            .unwrap_or_else(|| ModelProviderInfo::create_openai_provider(None));
        let source = Source {
            provider_id: config.provider_id(),
            base_url: provider.base_url.clone(),
            openai_auth: provider.requires_openai_auth,
            headers: provider_headers(&provider, &env),
            query: provider_query(&provider),
        };
        let catalog = Catalog::new(home.clone(), source, Arc::clone(account.manager()));
        Ok(Self {
            store_root: env.store_root(),
            env,
            home,
            overrides,
            account,
            catalog,
            base_instructions,
            configs: Mutex::new(HashMap::new()),
            models: Mutex::new(HashMap::new()),
        })
    }

    async fn config(&self, cwd: &Path) -> Arc<CodexConfig> {
        if let Some(config) = self
            .configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(cwd)
        {
            return Arc::clone(config);
        }
        self.load_config(cwd).await
    }

    async fn load_config(&self, cwd: &Path) -> Arc<CodexConfig> {
        let (home, directory, overrides) =
            (self.home.clone(), cwd.to_path_buf(), self.overrides.clone());
        let config = Arc::new(
            tokio::task::spawn_blocking(move || {
                CodexConfig::load(&home, Some(&directory), &overrides)
            })
            .await
            .unwrap_or_default(),
        );
        self.configs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(cwd.to_path_buf(), Arc::clone(&config));
        config
    }

    fn metadata(&self, model: &str) -> Option<Arc<CodexModel>> {
        self.models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(model)
            .cloned()
    }

    /// Body settings the model takes: its verbosity and its reasoning summary.
    fn body(&self, config: &CodexConfig, model: &str) -> Map<String, Value> {
        let mut body = Map::new();
        let Some(info) = self.metadata(model) else {
            return body;
        };
        if info.support_verbosity
            && let Some(verbosity) = config.verbosity().or_else(|| {
                info.default_verbosity
                    .map(|verbosity| verbosity.to_string())
            })
        {
            body.insert("text".to_owned(), json!({"verbosity": verbosity}));
        }
        let summary = config
            .summary()
            .unwrap_or_else(|| info.default_reasoning_summary.to_string());
        if info.supports_reasoning_summary_parameter && !matches!(summary.as_str(), "auto" | "none")
        {
            body.insert("reasoning".to_owned(), json!({"summary": summary}));
        }
        body
    }
}

#[async_trait]
impl Flavor for CodexFlavor {
    fn identity(&self) -> Identity {
        // The wire name is what Nori keys Codex-specific behaviour on; keep it.
        Identity {
            name: "codex-acp",
            title: "Codex",
            store: "codex",
        }
    }

    async fn workspace(&self, cwd: &Path) -> Result<Workspace, AgentError> {
        let config = self.load_config(cwd).await;
        let offered = self
            .catalog
            .offered(
                config.model().as_deref(),
                config.effort().as_deref(),
                config.service_tier().as_deref(),
            )
            .await;
        self.models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(offered.metadata);
        let (home, directory, read) = (self.home.clone(), cwd.to_path_buf(), Arc::clone(&config));
        let (instructions, mut warnings) =
            tokio::task::spawn_blocking(move || instructions::find(&home, &directory, &read))
                .await
                .unwrap_or_default();
        warnings.extend(config.warnings.iter().cloned());
        if let Err(error) = config.providers() {
            warnings.push(error);
        }
        Ok(Workspace {
            models: offered.models,
            default_model: offered.default_model,
            modes: modes::modes(),
            default_mode: Some(modes::DEFAULT_MODE.to_owned()),
            rules: modes::base_rules(),
            instructions,
            commands: modes::commands(),
            mcp_servers: config.mcp_servers(&self.env),
            warnings,
            dials: vec![offered.service_tier],
        })
    }

    fn workspace_changes(&self) -> Option<tokio::sync::watch::Receiver<i64>> {
        Some(self.catalog.changes())
    }

    async fn connect(&self, cwd: &Path, model: &str) -> Result<Connection, AgentError> {
        let config = self.config(cwd).await;
        let provider_id = config.provider_id();
        let provider = config
            .providers()
            .map_err(AgentError::invalid)?
            .remove(&provider_id)
            .ok_or_else(|| {
                AgentError::invalid(format!(
                    "model_provider {provider_id:?} is not one of Codex's providers: add it under [model_providers] in config.toml"
                ))
            })?;
        let mut headers = provider_headers(&provider, &self.env);
        headers.push((
            "originator".to_owned(),
            codex_login::default_client::originator().value,
        ));
        headers.push((
            "user-agent".to_owned(),
            codex_login::default_client::get_codex_user_agent(),
        ));
        let (auth, base_url, account) = if provider.requires_openai_auth {
            let credential = self.account.credential().await?;
            if let Some(id) = &credential.account_id {
                headers.push(("chatgpt-account-id".to_owned(), id.clone()));
            }
            if credential.fedramp {
                headers.push(("X-OpenAI-Fedramp".to_owned(), "true".to_owned()));
            }
            let base_url = provider.base_url.clone().unwrap_or_else(|| {
                if credential.chatgpt {
                    CHATGPT_CODEX_BASE_URL.to_owned()
                } else {
                    OPENAI_API_BASE_URL.to_owned()
                }
            });
            (
                Auth::Bearer(Secret::new(credential.token)),
                base_url,
                credential.payer,
            )
        } else {
            let auth = match (&provider.env_key, &provider.experimental_bearer_token) {
                (Some(variable), _) => {
                    let key = self.env.get(variable).ok_or_else(|| {
                        AgentError::AuthRequired(match &provider.env_key_instructions {
                            Some(instructions) => format!("{variable} is not set. {instructions}"),
                            None => format!("{variable} is not set: {} needs it", provider.name),
                        })
                    })?;
                    Auth::Bearer(Secret::new(key))
                }
                (None, Some(token)) => Auth::Bearer(Secret::new(token.as_str())),
                (None, None) => Auth::None,
            };
            let base_url = provider.base_url.clone().ok_or_else(|| {
                AgentError::invalid(format!(
                    "model_provider {provider_id:?} has no base_url in config.toml"
                ))
            })?;
            (auth, base_url, provider.name.clone())
        };
        let mut endpoint = Endpoint::new(Wire::OpenAiResponses, base_url, auth);
        endpoint.headers = headers
            .into_iter()
            .map(|(name, value)| (name, Secret::new(value)))
            .collect();
        endpoint.query = provider_query(&provider);
        endpoint.timeouts.chunk = provider.stream_idle_timeout();
        Ok(Connection {
            endpoint,
            model: model.to_owned(),
            effort_style: EffortStyle::Named,
            reasoning_echo_field: None,
            extra_body: self.body(&config, model),
            temperature: None,
            account: Some(account),
            // Codex never states one, and ChatGPT's backend refuses it.
            output_limit: false,
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        self.account.status().await
    }

    fn system_prompt(&self, model: &ModelInfo) -> String {
        if let Some(instructions) = &self.base_instructions {
            return instructions.clone();
        }
        match self.metadata(&model.id) {
            Some(info) => codex_prompts::render_model_instructions(&info),
            None => codex_models_manager::model_info::BASE_INSTRUCTIONS.to_owned(),
        }
    }

    fn tools(&self) -> Vec<Arc<dyn tools::Tool>> {
        let mut plan = tools::Todo::new(tools::TodoStyle::UpdatePlan);
        UPDATE_PLAN_DESCRIPTION.clone_into(&mut plan.description);
        let mut tools: Vec<Arc<dyn tools::Tool>> =
            vec![Arc::new(tools::Shell::new(tools::ShellStyle::ShellCommand))];
        tools.extend(tools::SharedTool::catalog(tools::EditStyle::Patch));
        tools.extend([
            Arc::new(plan) as Arc<dyn tools::Tool>,
            Arc::new(tools::ViewImage::default()),
        ]);
        tools
    }

    /// Codex reads files through its shell, so an edit cannot be gated on a `read` call.
    fn file_tools(&self) -> tools::WorkspaceConfig {
        tools::WorkspaceConfig {
            require_read_before_edit: false,
            ..tools::WorkspaceConfig::default()
        }
    }

    fn rate_limits(&self, headers: &[(String, String)]) -> Vec<RateLimitWindow> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0));
        limits::windows(headers, now)
    }

    fn store_root(&self) -> PathBuf {
        self.store_root.clone()
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        self.account.methods()
    }

    async fn authenticate(&self, method: &str) -> Result<(), AgentError> {
        self.account.sign_in(method).await
    }

    fn can_logout(&self) -> bool {
        true
    }

    async fn logout(&self) -> Result<(), AgentError> {
        self.account.sign_out().await
    }

    fn session_headers(
        &self,
        session: &str,
        _endpoint: &nori_llm::Endpoint,
    ) -> Vec<(String, String)> {
        // Codex names the thread in every request; ChatGPT keys prompt-cache affinity on it.
        [
            "session_id",
            "session-id",
            "thread-id",
            "x-client-request-id",
        ]
        .into_iter()
        .map(|name| (name.to_owned(), session.to_owned()))
        .collect()
    }

    async fn reauthorize(&self) -> bool {
        self.account.reauthorize().await
    }
}
