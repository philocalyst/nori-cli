//! Starting a child: open its agent's session, select its model, and spawn its actor.

use std::sync::Arc;
use std::time::Duration;

use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;
use tokio::sync::mpsc;

use super::Child;
use super::Launched;
use super::actor::run_actor;
use crate::backend::orchestrator::Inner;

/// How long an agent may take to start and open a session before the task fails. Agents that
/// install themselves on first use (npx, uvx) need the margin.
const LAUNCH_TIMEOUT: Duration = Duration::from_secs(180);

/// What a child was started under: the role that selects its recovery policy, and what a program
/// restricts it to.
#[derive(Default)]
pub(in crate::backend::orchestrator) struct ChildTerms {
    pub(in crate::backend::orchestrator) role: Option<String>,
    pub(in crate::backend::orchestrator) scope:
        Option<Arc<crate::backend::orchestrator::ActorScope>>,
}

/// Start the agent, open a session, select `model`, and start the child's actor.
pub(in crate::backend::orchestrator) async fn launch(
    inner: &Arc<Inner>,
    spec: crate::runtime::AgentPrepareSpec,
    model: Option<&str>,
    options: &[(String, String)],
    staged: bool,
    scope: acp::SessionId,
    terms: ChildTerms,
) -> Result<Launched, String> {
    let ChildTerms {
        role,
        scope: program_scope,
    } = terms;
    let mut spec = spec;
    // A program's world gives the actor a Nori home of its own.
    if let Some(home) = program_scope.as_ref().and_then(|scope| scope.home.clone()) {
        let mut config = (*spec.config).clone();
        config.nori_home = home;
        spec.config = Arc::new(config);
    }
    let agent = spec.config.active_agent.clone();
    let nori_home = spec.config.nori_home.clone();
    let spec_approval_policy = spec.config.approval_policy;
    let mut child_seed = inner.seed.clone();
    child_seed.actor_view = Some(scope.clone());
    // The role selects the child's recovery policy.
    child_seed.role = role;
    // What the program restricts the actor to is the actor's own seed, so its tools and its
    // `bash` are served under it; without one it keeps what its parent's sandbox leaves it.
    // A child never has more freedom than the session that delegated to it: the sandbox of
    // the delegating session narrows its own.
    let inherited = child_seed
        .scope
        .as_ref()
        .and_then(|scope| scope.sandbox.clone());
    child_seed.scope = match program_scope {
        Some(restriction) => {
            let mut restriction = (*restriction).clone();
            restriction.sandbox = match (inherited.as_ref(), restriction.sandbox.take()) {
                (outer, Some(own)) => Some(nori_conduit::narrow_sandbox(outer, &own)),
                (Some(outer), None) => Some(outer.clone()),
                (None, None) => None,
            };
            Some(Arc::new(restriction))
        }
        None => inherited.map(|sandbox| {
            Arc::new(crate::backend::orchestrator::ActorScope {
                sandbox: Some(sandbox),
                ..Default::default()
            })
        }),
    };
    let launch = async {
        let agent = crate::runtime::prepare_delegated_agent(spec, &child_seed)
            .await
            .map_err(|error| format!("could not start the agent: {error:#}"))?;
        let mut session = crate::runtime::launch_session(crate::runtime::SessionLaunchSpec {
            agent,
            start: crate::runtime::SessionStart::New,
        });
        let started = loop {
            match session.events.recv().await {
                Some(SessionEvent::Nori(NoriEvent::SessionStarted(started))) => break started,
                Some(SessionEvent::Nori(NoriEvent::SessionEnded(ended))) => {
                    return Err(ended.message.unwrap_or_else(|| {
                        "the agent ended the session as it started".to_owned()
                    }));
                }
                Some(_) => {}
                None => return Err("the agent exited before its session started".to_owned()),
            }
        };
        Ok((session, started))
    };
    let (session, started) = tokio::time::timeout(LAUNCH_TIMEOUT, launch)
        .await
        .map_err(|_| {
            format!(
                "the agent did not open a session within {}s",
                LAUNCH_TIMEOUT.as_secs()
            )
        })??;
    crate::backend::orchestrator::catalog::remember(&nori_home, &agent, &started.config_options);
    let injects_model = crate::registry::get_agent_config(&agent)
        .is_ok_and(|agent| agent.supports_model_injection());
    let model = match model {
        Some(requested) => {
            match crate::backend::orchestrator::models::select_model(
                &session.handle,
                &started.config_options,
                requested,
                injects_model,
            )
            .await
            {
                Ok(model) => Some(model),
                Err(error) => {
                    let _ = session.handle.shutdown().await;
                    return Err(error);
                }
            }
        }
        None => crate::backend::orchestrator::models::current_model(&started.config_options),
    };
    // A permission mode can only be set when the parent runs without approvals, so a child is
    // never given one broader than its parent's.
    let may_set_mode = spec_approval_policy == nori_config::AskForApproval::Never;
    if let Err(error) = crate::backend::orchestrator::models::apply_options(
        &session.handle,
        &started.config_options,
        options,
        may_set_mode,
    )
    .await
    {
        let _ = session.handle.shutdown().await;
        return Err(error);
    }
    let (turns_tx, turns_rx) = mpsc::unbounded_channel();
    let child = Child {
        harness: session.handle.clone(),
        turns: turns_tx,
    };
    let acp_session = started.acp_session_id.clone();
    tokio::spawn(run_actor(
        Arc::downgrade(inner),
        session.handle,
        started.acp_session_id,
        session.events,
        turns_rx,
        staged,
        scope,
    ));
    Ok(Launched {
        child,
        model,
        session: acp_session,
    })
}
