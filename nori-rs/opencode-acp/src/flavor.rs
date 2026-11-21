//! OpenCode as an engine flavor: the gateway's models, an API key, OpenCode's prompt and tools,
//! with Nori's engine running the loop in Nori's process.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use nori_acp_kit::AgentError;
use nori_acp_kit::AuthStatus;
use nori_acp_kit::async_trait;
use nori_acp_kit::schema::AuthEnvVar;
use nori_acp_kit::schema::AuthMethod;
use nori_acp_kit::schema::AuthMethodEnvVar;
use nori_engine::Connection;
use nori_engine::Effect;
use nori_engine::EffortStyle;
use nori_engine::Flavor;
use nori_engine::Identity;
use nori_engine::Instruction;
use nori_engine::Mode;
use nori_engine::ModelInfo;
use nori_engine::Rule;
use nori_engine::Workspace;
use nori_engine::tools;
use nori_llm::Auth;
use nori_llm::Endpoint;
use nori_llm::Wire;

use crate::catalog::Catalog;
use crate::env::Env;
use crate::key;

/// The largest instruction file read whole.
const MAX_INSTRUCTION_BYTES: u64 = 256 * 1024;
/// The sign-in that keeps an API key (ACP `authenticate`).
const API_KEY_METHOD: &str = "opencode-api-key";

const BASE_PROMPT: &str = include_str!("prompt.md");
const PLAN_PROMPT: &str = "You are in plan mode. Investigate and plan; do not change files. Read, search and run read-only commands as needed, then present a concise plan of the changes you would make and wait for the user to approve it before changing anything.";

pub(crate) struct OpenCodeFlavor {
    env: Env,
    /// Where the account's key is kept.
    key_file: PathBuf,
    store_root: PathBuf,
    catalog: Catalog,
}

impl OpenCodeFlavor {
    pub(crate) fn new(env: Env, key_file: PathBuf) -> Self {
        Self {
            store_root: crate::env::store_root(&env),
            catalog: Catalog::shared(&env),
            env,
            key_file,
        }
    }

    async fn models(&self) -> Vec<crate::models::ModelEntry> {
        crate::models::models(&*self.catalog.models().await)
    }
}

fn modes() -> Vec<Mode> {
    vec![
        Mode {
            id: "build".to_owned(),
            label: "Build".to_owned(),
            description: Some("Every tool".to_owned()),
            rules: Vec::new(),
            prompt: None,
            model: None,
        },
        Mode {
            id: "plan".to_owned(),
            label: "Plan".to_owned(),
            description: Some("Read and plan; edits are off".to_owned()),
            rules: vec![Rule::new("edit", "*", Effect::Deny)],
            prompt: Some(PLAN_PROMPT.to_owned()),
            model: None,
        },
    ]
}

/// The project's instructions: `AGENTS.md` (else `CLAUDE.md`) in each directory from the project
/// root (the nearest one holding `.git`) down to `cwd`, outermost first.
fn instructions(cwd: &Path) -> Vec<Instruction> {
    let root = cwd
        .ancestors()
        .find(|directory| directory.join(".git").exists())
        .unwrap_or(cwd);
    let directories: Vec<&Path> = cwd
        .ancestors()
        .take_while(|directory| directory.starts_with(root))
        .collect();
    ["AGENTS.md", "CLAUDE.md"]
        .into_iter()
        .map(|name| {
            directories
                .iter()
                .rev()
                .map(|directory| directory.join(name))
                .filter(|path| {
                    std::fs::metadata(path).is_ok_and(|metadata| {
                        metadata.is_file() && metadata.len() <= MAX_INSTRUCTION_BYTES
                    })
                })
                .filter_map(|path| {
                    let text = std::fs::read_to_string(&path).ok()?;
                    Some(Instruction { source: path, text })
                })
                .collect::<Vec<_>>()
        })
        .find(|found| !found.is_empty())
        .unwrap_or_default()
}

#[async_trait]
impl Flavor for OpenCodeFlavor {
    fn identity(&self) -> Identity {
        Identity {
            name: "nori-opencode",
            title: "OpenCode",
            store: "opencode",
        }
    }

    async fn workspace(&self, cwd: &Path) -> Result<Workspace, AgentError> {
        let models: Vec<ModelInfo> = self
            .models()
            .await
            .into_iter()
            .map(|model| model.info)
            .collect();
        let directory = cwd.to_path_buf();
        let instructions = tokio::task::spawn_blocking(move || instructions(&directory))
            .await
            .unwrap_or_default();
        Ok(Workspace {
            default_model: models.first().map(|model| model.id.clone()),
            models,
            modes: modes(),
            default_mode: None,
            rules: Vec::new(),
            instructions,
            commands: Vec::new(),
            mcp_servers: Vec::new(),
            warnings: Vec::new(),
            dials: Vec::new(),
        })
    }

    fn workspace_changes(&self) -> Option<tokio::sync::watch::Receiver<i64>> {
        Some(self.catalog.changes())
    }

    fn session_headers(
        &self,
        session: &str,
        _endpoint: &nori_llm::Endpoint,
    ) -> Vec<(String, String)> {
        // The gateway routes and caches by conversation, and refuses a request that names none.
        vec![("x-opencode-session".to_owned(), session.to_owned())]
    }

    async fn connect(&self, _cwd: &Path, model: &str) -> Result<Connection, AgentError> {
        let models = self.models().await;
        let entry = models
            .iter()
            .find(|entry| entry.info.id == model)
            .ok_or_else(|| AgentError::Provider {
                kind: nori_acp_kit::FailureKind::ModelUnavailable,
                detail: format!("OpenCode's gateway does not list {model}"),
                resets_at: None,
                retry_after: None,
            })?;
        let (secret, source) = key::read(&self.env, &self.key_file).await.ok_or_else(|| {
            AgentError::AuthRequired(format!(
                "OpenCode needs an API key: add one (from opencode.ai/auth) to this account, or set {}",
                key::VARIABLE
            ))
        })?;
        let auth = match entry.wire {
            Wire::AnthropicMessages => Auth::Header {
                name: "x-api-key".to_owned(),
                value: secret,
            },
            Wire::OpenAiResponses | Wire::OpenAiChat => Auth::Bearer(secret),
        };
        let account = match source {
            key::Source::Environment => format!("OpenCode ({})", key::VARIABLE),
            key::Source::Kept => "OpenCode (API key)".to_owned(),
        };
        Ok(Connection {
            endpoint: Endpoint::new(entry.wire, entry.base_url.clone(), auth),
            model: entry.wire_model.clone(),
            effort_style: match entry.wire {
                Wire::AnthropicMessages => EffortStyle::AnthropicBudget,
                Wire::OpenAiResponses | Wire::OpenAiChat => EffortStyle::Named,
            },
            reasoning_echo_field: entry.reasoning_field.clone(),
            extra_body: serde_json::Map::new(),
            temperature: None,
            account: Some(account),
            output_limit: true,
        })
    }

    async fn auth_status(&self) -> AuthStatus {
        match key::read(&self.env, &self.key_file).await {
            Some((_, source)) => AuthStatus {
                kind: "api_key".to_owned(),
                label: match source {
                    key::Source::Environment => format!("OpenCode · {}", key::VARIABLE),
                    key::Source::Kept => "OpenCode · API key".to_owned(),
                },
                detail: None,
                account: None,
            },
            None => {
                let mut status = AuthStatus::signed_out();
                status.detail = Some("No OpenCode API key".to_owned());
                status
            }
        }
    }

    fn auth_methods(&self) -> Vec<AuthMethod> {
        vec![AuthMethod::EnvVar(
            AuthMethodEnvVar::new(
                API_KEY_METHOD,
                "OpenCode API key",
                vec![AuthEnvVar::new(key::VARIABLE)],
            )
            .description("A key from opencode.ai/auth, kept by Nori for this account"),
        )]
    }

    async fn authenticate(&self, method: &str) -> Result<(), AgentError> {
        if method != API_KEY_METHOD {
            return Err(AgentError::invalid(format!(
                "OpenCode signs in only with an API key, not {method}"
            )));
        }
        let given = self.env.get(key::VARIABLE).ok_or_else(|| {
            AgentError::AuthRequired(format!("No key was given in {}", key::VARIABLE))
        })?;
        key::keep(&self.key_file, &given).await.map_err(|error| {
            AgentError::invalid(format!("The OpenCode key could not be kept: {error}"))
        })
    }

    fn can_logout(&self) -> bool {
        true
    }

    async fn logout(&self) -> Result<(), AgentError> {
        key::forget(&self.key_file).await.map_err(|error| {
            AgentError::invalid(format!("The OpenCode key could not be removed: {error}"))
        })
    }

    fn system_prompt(&self, model: &ModelInfo) -> String {
        BASE_PROMPT.replace("{{MODEL}}", &model.label)
    }

    fn tools(&self) -> Vec<Arc<dyn tools::Tool>> {
        let mut tools: Vec<Arc<dyn tools::Tool>> =
            vec![Arc::new(tools::Shell::new(tools::ShellStyle::Bash))];
        tools.extend(tools::SharedTool::catalog(tools::EditStyle::Replace));
        tools.extend([
            Arc::new(tools::Glob::default()) as Arc<dyn tools::Tool>,
            Arc::new(tools::Grep::default()),
            Arc::new(tools::List::default()),
            Arc::new(tools::Todo::new(tools::TodoStyle::TodoWrite)),
        ]);
        tools
    }

    fn store_root(&self) -> PathBuf {
        self.store_root.clone()
    }
}
