//! Signing in to Codex: the authentication methods the agent offers, `authenticate`, and the check
//! that a session can start.

use codex_login::CODEX_API_KEY_ENV_VAR;
use codex_login::OPENAI_API_KEY_ENV_VAR;
use codex_login::auth::CodexAuth;
use codex_login::auth::read_codex_api_key_from_env;
use codex_login::auth::read_openai_api_key_from_env;
use nori_acp_kit::AgentError;
use nori_acp_kit::AuthAccount;
use nori_acp_kit::AuthStatus;
use nori_acp_kit::acp;
use nori_acp_kit::announce_auth_status;
use nori_acp_kit::schema::AuthEnvVar;
use nori_acp_kit::schema::AuthMethod;
use nori_acp_kit::schema::AuthMethodAgent;
use nori_acp_kit::schema::AuthMethodEnvVar;
use nori_acp_kit::schema::AuthMethodId;
use nori_acp_kit::schema::AuthenticateRequest;
use nori_acp_kit::schema::AuthenticateResponse;
use nori_acp_kit::schema::LogoutResponse;

use super::CodexAgent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CodexAuthMethod {
    ChatGpt,
    CodexApiKey,
    OpenAiApiKey,
}

/// The methods the agent offers.
pub(super) fn auth_methods() -> Vec<AuthMethod> {
    let mut methods = vec![
        CodexAuthMethod::ChatGpt.into(),
        CodexAuthMethod::CodexApiKey.into(),
        CodexAuthMethod::OpenAiApiKey.into(),
    ];
    // Until codex device code auth works, we can't use this in remote ssh projects
    if std::env::var("NO_BROWSER").is_ok() {
        methods.remove(0);
    }
    methods
}

impl CodexAgent {
    /// Whether a session can start: the account is signed in, or the provider needs no account.
    pub(super) async fn check_auth(&self) -> Result<(), AgentError> {
        if self.config.model_provider_id == "openai"
            && self.auth_manager.auth().await.is_none()
            // Check if anything changed on disk since the last reload
            && !self.auth_manager.reload().await
        {
            return Err(AgentError::AuthRequired(format!(
                "Codex is not signed in. Log in with ChatGPT, or set {CODEX_API_KEY_ENV_VAR} or \
                 {OPENAI_API_KEY_ENV_VAR}."
            )));
        }
        Ok(())
    }

    /// Tell the client who this Codex is signed in as: the ChatGPT email and plan the login's
    /// own token states, or that an API key pays. Nothing from the credential beyond that.
    pub(super) async fn announce_identity(&self, cx: &nori_acp_kit::Cx) {
        let status = match self.auth_manager.auth().await {
            Some(auth) if auth.is_chatgpt_auth() => {
                let plan = auth
                    .account_plan_type()
                    .map(|plan| format!("{plan:?}").to_lowercase());
                AuthStatus::account(
                    plan.as_deref()
                        .map_or_else(|| "ChatGPT".to_owned(), |plan| format!("ChatGPT {plan}")),
                    AuthAccount {
                        email: auth.get_account_email(),
                        organization: None,
                        plan,
                    },
                )
            }
            Some(_) => AuthStatus {
                kind: "api_key".to_owned(),
                label: "OpenAI API key".to_owned(),
                detail: None,
                account: None,
            },
            None => AuthStatus::signed_out(),
        };
        if let Err(error) = announce_auth_status(cx, &status) {
            tracing::debug!(%error, "account identity not sent");
        }
    }

    pub(super) async fn sign_in(
        &self,
        request: AuthenticateRequest,
    ) -> acp::Result<AuthenticateResponse> {
        let auth_method = CodexAuthMethod::try_from(request.method_id)?;

        // Check before starting login flow if already authenticated with the same method
        if let Some(auth) = self.auth_manager.auth().await {
            match (auth, auth_method) {
                (
                    CodexAuth::ApiKey(..),
                    CodexAuthMethod::CodexApiKey | CodexAuthMethod::OpenAiApiKey,
                )
                | (CodexAuth::Chatgpt(..), CodexAuthMethod::ChatGpt) => {
                    return Ok(AuthenticateResponse::new());
                }
                _ => {}
            }
        }

        match auth_method {
            CodexAuthMethod::ChatGpt => {
                // Perform browser/device login via codex-rs, then report success/failure to the client.
                let opts = codex_login::ServerOptions::new(
                    self.config.codex_home.to_path_buf(),
                    codex_login::auth::CLIENT_ID.to_string(),
                    self.config.auth_config().effective_chatgpt_workspaces(),
                    self.config.cli_auth_credentials_store_mode,
                    self.config.auth_keyring_backend_kind(),
                    self.config.auth_route_config(),
                );

                let server = codex_login::run_login_server(opts).map_err(AgentError::internal)?;

                server
                    .block_until_done()
                    .await
                    .map_err(AgentError::internal)?;
            }
            CodexAuthMethod::CodexApiKey => {
                let api_key = read_codex_api_key_from_env().ok_or_else(|| {
                    AgentError::internal(format!("{CODEX_API_KEY_ENV_VAR} is not set"))
                })?;
                self.save_api_key(&api_key)?;
            }
            CodexAuthMethod::OpenAiApiKey => {
                let api_key = read_openai_api_key_from_env().ok_or_else(|| {
                    AgentError::internal(format!("{OPENAI_API_KEY_ENV_VAR} is not set"))
                })?;
                self.save_api_key(&api_key)?;
            }
        }

        self.auth_manager.reload().await;

        Ok(AuthenticateResponse::new())
    }

    fn save_api_key(&self, api_key: &str) -> Result<(), AgentError> {
        codex_login::login_with_api_key(
            &self.config.codex_home,
            api_key,
            self.config.cli_auth_credentials_store_mode,
            self.config.auth_keyring_backend_kind(),
        )
        .map_err(AgentError::internal)
    }

    pub(super) async fn sign_out(&self) -> acp::Result<LogoutResponse> {
        self.auth_manager
            .logout()
            .await
            .map_err(AgentError::internal)?;
        Ok(LogoutResponse::new())
    }
}

impl From<CodexAuthMethod> for AuthMethodId {
    fn from(method: CodexAuthMethod) -> Self {
        Self::new(match method {
            CodexAuthMethod::ChatGpt => "chatgpt",
            CodexAuthMethod::CodexApiKey => "codex-api-key",
            CodexAuthMethod::OpenAiApiKey => "openai-api-key",
        })
    }
}

impl From<CodexAuthMethod> for AuthMethod {
    fn from(method: CodexAuthMethod) -> Self {
        match method {
            CodexAuthMethod::ChatGpt => Self::Agent(
                AuthMethodAgent::new(method, "Login with ChatGPT").description(
                    "Use your ChatGPT login with Codex CLI (requires a paid ChatGPT subscription)",
                ),
            ),
            CodexAuthMethod::CodexApiKey => Self::EnvVar(
                AuthMethodEnvVar::new(
                    method,
                    format!("Use {CODEX_API_KEY_ENV_VAR}"),
                    vec![AuthEnvVar::new(CODEX_API_KEY_ENV_VAR)],
                )
                .description(format!(
                    "Requires setting the `{CODEX_API_KEY_ENV_VAR}` environment variable."
                )),
            ),
            CodexAuthMethod::OpenAiApiKey => Self::EnvVar(
                AuthMethodEnvVar::new(
                    method,
                    format!("Use {OPENAI_API_KEY_ENV_VAR}"),
                    vec![AuthEnvVar::new(OPENAI_API_KEY_ENV_VAR)],
                )
                .description(format!(
                    "Requires setting the `{OPENAI_API_KEY_ENV_VAR}` environment variable."
                )),
            ),
        }
    }
}

impl TryFrom<AuthMethodId> for CodexAuthMethod {
    type Error = AgentError;

    fn try_from(value: AuthMethodId) -> Result<Self, Self::Error> {
        match value.0.as_ref() {
            "chatgpt" => Ok(CodexAuthMethod::ChatGpt),
            "codex-api-key" => Ok(CodexAuthMethod::CodexApiKey),
            "openai-api-key" => Ok(CodexAuthMethod::OpenAiApiKey),
            other => Err(AgentError::invalid(format!(
                "unsupported authentication method {other:?}"
            ))),
        }
    }
}
