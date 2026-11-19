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
//! An agent that runs inside Nori has no launch command for a terminal method. When it offers no
//! method Nori can run, the account's own login program (`accounts::login_program`: the provider's
//! login CLI, unwrapped, with the account's home) is its terminal method.
//!
//! Signing in is only reported once a fresh connection to the agent can open a session.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use nori_config::AcpProxyConfig;
use nori_protocol::acp::v1 as acp;

use crate::connection::acp_connection::AcpConnection;
use crate::registry::AcpAgentConfig;
use nori_protocol::AcpEvent;

/// One way an agent offers to sign in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignInMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub how: SignInHow,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInHow {
    /// Run this where the user can answer it.
    Terminal {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Ask the agent to sign in by itself.
    Agent,
}

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
    // may offer no method at all: its account's own login program, run with the account's home,
    // signs it in instead.
    if config.in_process.is_some()
        && methods.is_empty()
        && let Some(program) = crate::accounts::login_program(&config.provider_slug)
    {
        methods.push(SignInMethod {
            id: "login".to_owned(),
            name: format!("Sign in to {}", config.display_name),
            description: None,
            how: SignInHow::Terminal {
                command: program.command,
                args: program.args,
                env: program.env,
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
                how: SignInHow::Terminal {
                    command: config.command.clone(),
                    args,
                    env,
                },
            })
        }
        acp::AuthMethod::Agent(agent) => {
            let wants_key = agent
                .meta
                .as_ref()
                .is_some_and(|meta| meta.contains_key("api-key"));
            (!wants_key).then_some(SignInMethod {
                id,
                name,
                description: agent.description,
                how: SignInHow::Agent,
            })
        }
        // An environment-variable method is a secret the user configures, not a sign-in.
        _ => None,
    }
}

/// Ask the agent to sign in by itself with `method_id`, and wait until it says it has.
pub async fn authenticate(config: &AcpAgentConfig, cwd: &Path, method_id: &str) -> Result<()> {
    let connection = AcpConnection::spawn(config, cwd, AcpProxyConfig::disabled()).await?;
    let result = connection
        .send_ext_request("authenticate", serde_json::json!({ "methodId": method_id }))
        .await;
    connection.shutdown().await;
    result.map(drop)
}

/// Whether the agent can now open a session: the only proof that signing in took.
pub async fn verify(config: &AcpAgentConfig, cwd: &Path) -> Result<()> {
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
