//! Signing in through the agent itself.
//!
//! An agent knows where its credentials live: a Claude adapter started through a profile wrapper
//! signs in to that profile, a Codex adapter to its `CODEX_HOME`. So Nori never runs a guessed
//! login command of its own. It asks the agent (`initialize`, declaring that it can run a
//! terminal for sign-in) and does what the agent says:
//!
//! - a **terminal** method is the agent's own launch command (the same command, arguments and
//!   environment Nori starts it with) plus the method's arguments and environment, run where the
//!   user can see and answer it;
//! - an **agent** method is `authenticate` on a connection to the agent, which signs in by
//!   itself (opening a browser, say) and answers when it is done.
//!
//! An agent that runs inside Nori has no launch command for a terminal method, and offers few
//! ways of its own. Nori adds the provider's own ways, run against the account's home
//! (`accounts::methods_for`: Claude's subscription, email, SSO, Console, token and key; Codex's
//! device code and key; OpenCode's key), and each can ask first for what it needs (an email, a
//! key, see `SignInAsk`).
//!
//! Signing in is only reported once the account's own login says it is signed in
//! (`accounts::status`): "the agent could open a session" proves nothing, since an agent opens
//! one without a login.

use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use nori_config::AcpProxyConfig;
use nori_protocol::acp::v1 as acp;

pub use crate::accounts::Answer;
pub use crate::accounts::Capture;
use crate::accounts::Secret;
pub use crate::accounts::SignInAsk;
pub use crate::accounts::SignInHow;
pub use crate::accounts::SignInMethod;
use crate::connection::acp_connection::AcpConnection;
use crate::registry::AcpAgentConfig;
use nori_protocol::AcpEvent;

/// How long an agent may take to say how it signs in.
const ASK_TIMEOUT: Duration = Duration::from_secs(30);

/// The ways `config`'s agent offers to sign in, in the order it lists them.
pub async fn sign_in_methods(config: &AcpAgentConfig, cwd: &Path) -> Result<Vec<SignInMethod>> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(4);
    let connection = tokio::time::timeout(
        ASK_TIMEOUT,
        AcpConnection::spawn_with_initialize_event_sender(
            config,
            cwd,
            AcpProxyConfig::disabled(),
            tx,
        ),
    )
    .await
    .context("the agent did not start in time")??;
    let response = match rx.recv().await {
        Some(AcpEvent::Response {
            response: Ok(acp::AgentResponse::InitializeResponse(response)),
            ..
        }) => response,
        Some(AcpEvent::Response {
            response: Err(error),
            ..
        }) => {
            connection.shutdown().await;
            anyhow::bail!("the agent refused to start: {error}");
        }
        Some(_) | None => {
            connection.shutdown().await;
            anyhow::bail!("the agent did not say how it signs in");
        }
    };
    connection.shutdown().await;
    let mut methods: Vec<SignInMethod> = response
        .auth_methods
        .into_iter()
        .filter_map(|method| method_for(config, method))
        .collect();
    // An agent in Nori's own process has no launch command to run a terminal method with, and
    // offers few ways of its own: the provider's own ways, run with the account's home, are
    // added, each asking first for what it needs. An agent that runs as a process and offers
    // nothing Nori can run gets them too (Antigravity's `agy acp --login`).
    if config.in_process.is_some() || methods.is_empty() {
        methods.extend(crate::accounts::methods_for(&config.provider_slug).await);
    }
    if config.in_process.is_some()
        && methods.is_empty()
        && let Some(program) = crate::accounts::login_program(&config.provider_slug)
    {
        methods.push(SignInMethod {
            id: "login".to_owned(),
            name: format!("Sign in to {}", config.display_name),
            description: None,
            ask: None,
            how: SignInHow::Terminal {
                command: program.command,
                args: program.args,
                env: program.env,
                capture: None,
            },
        });
    }
    Ok(methods)
}

/// The way to run `method`, when Nori can run it: a method that needs a secret typed in (an API
/// key) is left to the agent's own configuration.
fn method_for(config: &AcpAgentConfig, method: acp::AuthMethod) -> Option<SignInMethod> {
    let id = method.id().to_string();
    let name = method.name().to_owned();
    match method {
        // A terminal method is the agent's launch command plus its arguments; an agent in Nori's
        // own process has no launch command.
        acp::AuthMethod::Terminal(_) if config.in_process.is_some() => None,
        acp::AuthMethod::Terminal(terminal) => {
            let mut args = config.args.clone();
            args.extend(terminal.args);
            let mut env = config.env.clone();
            env.extend(terminal.env);
            Some(SignInMethod {
                id,
                name,
                description: terminal.description,
                ask: None,
                how: SignInHow::Terminal {
                    command: config.command.clone(),
                    args,
                    env,
                    capture: None,
                },
            })
        }
        acp::AuthMethod::Agent(agent) => {
            // A key is read from the agent's environment, and a gateway needs a base URL: neither
            // is something Nori can ask for and have last (Gemini's `gemini-api-key`, `gateway`).
            let wants_setup = agent
                .meta
                .as_ref()
                .is_some_and(|meta| meta.contains_key("api-key") || meta.contains_key("gateway"));
            (!wants_setup).then_some(SignInMethod {
                id,
                name,
                description: agent.description,
                ask: None,
                how: SignInHow::Agent { secret: None },
            })
        }
        // An environment-variable method is a secret the user configures, not a sign-in.
        _ => None,
    }
}

/// Ask the agent to sign in by itself with `method_id`, and wait until it says it has. A
/// `secret` (a key the user typed) is in the agent's environment for this one connection, so the
/// agent keeps it where its provider does and Nori keeps nothing.
pub async fn authenticate(
    config: &AcpAgentConfig,
    cwd: &Path,
    method_id: &str,
    secret: Option<&(String, Secret)>,
) -> Result<()> {
    let mut config = config.clone();
    if let Some((variable, value)) = secret {
        config
            .env
            .insert(variable.clone(), value.expose().to_owned());
    }
    let connection = AcpConnection::spawn(&config, cwd, AcpProxyConfig::disabled()).await?;
    let result = connection
        .send_ext_request("authenticate", serde_json::json!({ "methodId": method_id }))
        .await;
    connection.shutdown().await;
    result.map(drop)
}

/// Whether the account is now signed in, as its own login says (`accounts::status`): the only
/// proof that signing in took. For a provider Nori has no check for, a fresh agent that can open a
/// session is the best there is.
pub async fn verify(config: &AcpAgentConfig, cwd: &Path) -> Result<()> {
    let slug = &config.provider_slug;
    if crate::accounts::find(slug)
        .is_some_and(|account| crate::accounts::status::can_check(&account))
    {
        return match crate::accounts::status::check(slug).await {
            crate::accounts::AccountStatus::SignedIn(_) => Ok(()),
            crate::accounts::AccountStatus::NotSignedIn(reason) => anyhow::bail!("{reason}"),
            crate::accounts::AccountStatus::Unknown(reason) => {
                anyhow::bail!("Nori could not confirm the sign-in: {reason}")
            }
        };
    }
    let connection = tokio::time::timeout(
        ASK_TIMEOUT,
        AcpConnection::spawn(config, cwd, AcpProxyConfig::disabled()),
    )
    .await
    .context("the agent did not start in time")??;
    let session = connection.create_session(cwd, Vec::new()).await;
    connection.shutdown().await;
    session.map(drop)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    /// The `authMethods` of `gemini --acp` 0.47.0 and of `agy acp` 1.5.13, as they answered
    /// `initialize` in an empty home.
    fn offered(methods: serde_json::Value) -> Vec<String> {
        let config = crate::registry::get_agent_config("gemini").expect("gemini config");
        serde_json::from_value::<Vec<acp::AuthMethod>>(methods)
            .expect("auth methods")
            .into_iter()
            .filter_map(|method| method_for(&config, method))
            .map(|method| method.id)
            .collect()
    }

    #[test]
    fn geminis_key_and_gateway_are_configuration_not_sign_ins() {
        let offered = offered(json!([
            {"id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account"},
            {"id": "gemini-api-key", "name": "Gemini API key", "description": "Use an API key with Gemini Developer API", "_meta": {"api-key": {"provider": "google"}}},
            {"id": "vertex-ai", "name": "Vertex AI", "description": "Use an API key with Vertex AI GenAI API"},
            {"id": "gateway", "name": "AI API Gateway", "description": "Use a custom AI API Gateway", "_meta": {"gateway": {"protocol": "google", "restartRequired": "false"}}}
        ]));
        assert_eq!(offered, ["oauth-personal", "vertex-ai"]);
    }

    #[test]
    fn antigravitys_four_ways_are_all_offered() {
        let offered = offered(json!([
            {"id": "oauth-personal", "name": "Log in with Google", "description": "Log in with your Google account"},
            {"id": "oauth-business", "name": "Log in with Gemini Enterprise", "description": "Log in with your Gemini Enterprise account"},
            {"id": "gemini-api-key", "name": "Gemini API key", "description": "Use an API key with Gemini Developer API"},
            {"id": "agent-platform", "name": "Gemini Enterprise Agent Platform", "description": "Use Gemini Enterprise Agent Platform (formerly Vertex AI) with Application Default Credentials"}
        ]));
        assert_eq!(
            offered,
            [
                "oauth-personal",
                "oauth-business",
                "gemini-api-key",
                "agent-platform"
            ]
        );
    }
}
