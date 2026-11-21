//! Codex's sign-in: Codex's own `AuthManager` (`codex-login`) over the account's `CODEX_HOME`.
//!
//! The credential lives where Codex keeps it (`auth.json` or the keyring, as
//! `cli_auth_credentials_store` says). A ChatGPT access token close to expiry is refreshed by
//! `AuthManager::auth` before a request and written back to the account's home, so the Codex CLI
//! and Nori share one login. A token the backend refuses is recovered as Codex recovers it: the
//! login is read again from disk (another program may have refreshed it), then refreshed.

use std::path::PathBuf;
use std::sync::Arc;

use codex_config::ManagedAuthPolicy;
use codex_config::types::AuthCredentialsStoreMode;
use codex_config::types::AuthKeyringBackendKind;
use codex_http_client::HttpClientFactory;
use codex_http_client::OutboundProxyPolicy;
use codex_login::AuthConfig;
use codex_login::AuthManager;
use codex_login::AuthRouteConfig;
use codex_login::CODEX_API_KEY_ENV_VAR;
use codex_login::CodexAuth;
use codex_login::OPENAI_API_KEY_ENV_VAR;
use nori_acp_kit::AgentError;
use nori_acp_kit::AuthAccount;
use nori_acp_kit::AuthStatus;
use nori_acp_kit::schema::AuthEnvVar;
use nori_acp_kit::schema::AuthMethod;
use nori_acp_kit::schema::AuthMethodAgent;
use nori_acp_kit::schema::AuthMethodEnvVar;

use super::config::CodexConfig;
use super::env::Env;

const CHATGPT: &str = "chatgpt";
const CODEX_API_KEY: &str = "codex-api-key";
const OPENAI_API_KEY: &str = "openai-api-key";

/// What a request is sent with.
pub(crate) struct Credential {
    /// The bearer token: a ChatGPT access token or an API key.
    pub(crate) token: String,
    /// The ChatGPT workspace the token belongs to (`chatgpt-account-id`).
    pub(crate) account_id: Option<String>,
    /// A ChatGPT login (the Codex backend), rather than an API key.
    pub(crate) chatgpt: bool,
    pub(crate) fedramp: bool,
    /// Who pays, for the usage report.
    pub(crate) payer: String,
}

/// One account's Codex login.
pub(crate) struct Account {
    manager: Arc<AuthManager>,
    home: PathBuf,
    store_mode: AuthCredentialsStoreMode,
    workspaces: Option<Vec<String>>,
    route: AuthRouteConfig,
    env: Env,
}

impl Account {
    /// The login in `home`, with the storage and restrictions `config` names.
    pub(crate) async fn open(
        home: PathBuf,
        config: &CodexConfig,
        env: Env,
    ) -> std::io::Result<Self> {
        let route = AuthRouteConfig::from_http_client_factory(HttpClientFactory::new(
            OutboundProxyPolicy::ReqwestDefault,
        ));
        let store_mode = config.store_mode();
        let workspaces = config.forced_workspaces();
        let manager = AuthManager::shared_from_auth_config(
            AuthConfig {
                codex_home: home.clone(),
                auth_credentials_store_mode: store_mode,
                keyring_backend_kind: AuthKeyringBackendKind::default(),
                forced_login_method: None,
                chatgpt_base_url: config.chatgpt_base_url(),
                forced_chatgpt_workspace_id: workspaces.clone(),
                managed_auth_policy: ManagedAuthPolicy::default(),
                auth_route_config: route.clone(),
            },
            /*enable_codex_api_key_env*/ false,
        )
        .await
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
        Ok(Self {
            manager,
            home,
            store_mode,
            workspaces,
            route,
            env,
        })
    }

    pub(crate) fn manager(&self) -> &Arc<AuthManager> {
        &self.manager
    }

    /// The credential for the next request, refreshed first when it is about to expire.
    pub(crate) async fn credential(&self) -> Result<Credential, AgentError> {
        let auth = self.manager.auth().await.ok_or_else(|| {
            AgentError::AuthRequired(format!(
                "Codex is not signed in. Log in with ChatGPT, or set {CODEX_API_KEY_ENV_VAR} or {OPENAI_API_KEY_ENV_VAR}."
            ))
        })?;
        let token = auth.get_token().map_err(|error| {
            AgentError::AuthRequired(format!("Codex's sign-in cannot be used: {error}"))
        })?;
        let chatgpt = auth.uses_codex_backend();
        Ok(Credential {
            token,
            account_id: auth.get_account_id(),
            chatgpt,
            fedramp: auth.is_fedramp_account(),
            payer: if chatgpt {
                auth.get_account_email()
                    .unwrap_or_else(|| "ChatGPT".to_owned())
            } else {
                "OpenAI API key".to_owned()
            },
        })
    }

    /// Recover from a refused token as Codex does: read the login again, then refresh it.
    /// Whether the credential changed.
    pub(crate) async fn reauthorize(&self) -> bool {
        let mut recovery = self.manager.unauthorized_recovery();
        while recovery.has_next() {
            match recovery.next().await {
                Ok(step) if step.auth_state_changed() == Some(true) => return true,
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "Codex's sign-in could not be renewed");
                    return false;
                }
            }
        }
        false
    }

    /// Who the login is: the ChatGPT email and plan its own token states, or that an API key
    /// pays. Nothing from the credential beyond that.
    pub(crate) async fn status(&self) -> AuthStatus {
        match self.manager.auth().await {
            // `auth()` renews a login about to expire; one that is past its end afterwards could
            // not be renewed (its refresh token was refused or the network is gone), so it is
            // not a login that works.
            Some(auth) if auth.is_chatgpt_auth() && token_expired(&auth) => {
                let mut status = AuthStatus::signed_out();
                status.detail =
                    Some("the ChatGPT sign-in has expired and could not be renewed".to_owned());
                status
            }
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
        }
    }

    pub(crate) fn methods(&self) -> Vec<AuthMethod> {
        let mut methods = Vec::new();
        // A browser sign-in cannot finish on a machine without one.
        if self.env.get("NO_BROWSER").is_none() {
            methods.push(AuthMethod::Agent(
                AuthMethodAgent::new(CHATGPT, "Login with ChatGPT").description(
                    "Use your ChatGPT login with Codex CLI (requires a paid ChatGPT subscription)",
                ),
            ));
        }
        for (id, variable) in [
            (CODEX_API_KEY, CODEX_API_KEY_ENV_VAR),
            (OPENAI_API_KEY, OPENAI_API_KEY_ENV_VAR),
        ] {
            methods.push(AuthMethod::EnvVar(
                AuthMethodEnvVar::new(
                    id,
                    format!("Use {variable}"),
                    vec![AuthEnvVar::new(variable)],
                )
                .description(format!(
                    "Requires setting the `{variable}` environment variable."
                )),
            ));
        }
        methods
    }

    /// Sign in with `method`. A login of that kind already in place is kept.
    pub(crate) async fn sign_in(&self, method: &str) -> Result<(), AgentError> {
        let current = self.manager.auth().await;
        match (method, current.as_ref()) {
            (CHATGPT, Some(auth)) if auth.is_chatgpt_auth() => return Ok(()),
            (CODEX_API_KEY | OPENAI_API_KEY, Some(CodexAuth::ApiKey(_))) => return Ok(()),
            _ => {}
        }
        match method {
            CHATGPT => {
                let options = codex_login::ServerOptions::new(
                    self.home.clone(),
                    codex_login::CLIENT_ID.to_owned(),
                    self.workspaces.clone(),
                    self.store_mode,
                    AuthKeyringBackendKind::default(),
                    self.route.clone(),
                );
                let server =
                    codex_login::run_login_server(options).map_err(AgentError::internal)?;
                server
                    .block_until_done()
                    .await
                    .map_err(AgentError::internal)?;
            }
            CODEX_API_KEY | OPENAI_API_KEY => {
                let variable = if method == CODEX_API_KEY {
                    CODEX_API_KEY_ENV_VAR
                } else {
                    OPENAI_API_KEY_ENV_VAR
                };
                let key = self
                    .env
                    .get(variable)
                    .ok_or_else(|| AgentError::AuthRequired(format!("{variable} is not set")))?;
                let (home, store_mode) = (self.home.clone(), self.store_mode);
                tokio::task::spawn_blocking(move || {
                    codex_login::login_with_api_key(
                        &home,
                        &key,
                        store_mode,
                        AuthKeyringBackendKind::default(),
                    )
                })
                .await
                .map_err(AgentError::internal)?
                .map_err(AgentError::internal)?;
            }
            other => {
                return Err(AgentError::invalid(format!(
                    "unsupported authentication method {other:?}"
                )));
            }
        }
        self.manager.reload().await;
        Ok(())
    }

    pub(crate) async fn sign_out(&self) -> Result<(), AgentError> {
        self.manager.logout().await.map_err(AgentError::internal)?;
        Ok(())
    }
}

/// Whether the ChatGPT access token of `auth` is past its own `exp`. A token whose expiry cannot
/// be read is taken as not expired: the check says only what it can see.
fn token_expired(auth: &CodexAuth) -> bool {
    let Ok(token) = auth.get_token() else {
        return false;
    };
    jwt_expiry(&token).is_some_and(|expires| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0));
        expires <= now
    })
}

/// The `exp` (seconds since the epoch) a JWT states.
fn jwt_expiry(jwt: &str) -> Option<i64> {
    use base64::Engine as _;
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()?
        .get("exp")?
        .as_i64()
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use pretty_assertions::assert_eq;

    use super::*;

    fn jwt(payload: &str) -> String {
        let encode = |text: &str| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text);
        format!("{}.{}.sig", encode("{}"), encode(payload))
    }

    #[test]
    fn a_jwt_states_its_expiry() {
        assert_eq!(
            jwt_expiry(&jwt(r#"{"exp":1700000000}"#)),
            Some(1_700_000_000)
        );
    }

    #[test]
    fn a_token_that_is_not_a_jwt_has_no_expiry() {
        assert_eq!(jwt_expiry("sk-proj-not-a-jwt"), None);
        assert_eq!(jwt_expiry(&jwt(r#"{"sub":"x"}"#)), None);
    }
}
