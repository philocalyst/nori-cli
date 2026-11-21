//! Actors in a run: starting them, delivering messages and collecting replies.
//!
//! An actor's turns run in its own session, where the harness's recovery engine retries and
//! moves the actor along its fallback chain. The run hands that engine the program's,
//! the actor's and the enclosing steps' `recovery` overrides with every message
//! ([`recovery_for`]) and journals what the engine did ([`journal_recovery`]). What reaches
//! the run is a turn the engine gave up on: it fails with the final failure kind, and the
//! program's own `Retry` and `Recover` steps decide what happens next.

use std::sync::Arc;

use nori_workflow::RecoveryOverride;
use serde_json::json;

use super::env::ActorRuntime;
use super::env::Env;
use super::env::PendingTurn;
use super::env::Scope;
use crate::error::ConduitError;
use crate::host::ActorRef;
use crate::host::ActorSpec;
use crate::host::Delivery;
use crate::host::ProgramHost;
use crate::host::Receipt;
use crate::host::RecoveryNote;
use crate::host::Reply;
use crate::op::Target;
use crate::policy::narrow_sandbox;
use crate::wire::WireOp;

/// Stops a turn nobody waits for any more (a race lost, a step timed out, the run cancelled).
struct InFlight {
    host: Arc<dyn ProgramHost>,
    receipt: Option<Receipt>,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if let (Some(receipt), Ok(runtime)) =
            (self.receipt.take(), tokio::runtime::Handle::try_current())
        {
            let host = Arc::clone(&self.host);
            runtime.spawn(async move {
                let _ = host.cancel(&receipt).await;
            });
        }
    }
}

fn actor_name(actor: &ActorRef) -> String {
    match &actor.model {
        Some(model) => format!("{} · {model}", actor.agent),
        None => actor.agent.clone(),
    }
}

/// What the program, the actor and the enclosing steps say about recovery for work sent to
/// `alias`, least specific first. The host lays this over the policy in force.
fn recovery_for(env: &Env, alias: &str, scope: &Scope) -> Result<RecoveryOverride, ConduitError> {
    let declared = env.program.actors.get(alias);
    let mut over = RecoveryOverride::default();
    let blocks = [
        env.program.recovery.as_ref(),
        declared.and_then(|actor| actor.recovery.as_ref()),
    ];
    for block in blocks.into_iter().flatten() {
        over.merge(
            block
                .to_override()
                .map_err(|message| ConduitError::Runtime {
                    message: format!("recovery of `{alias}`: {message}"),
                })?,
        );
    }
    over.merge(scope.recovery.clone());
    Ok(over)
}

/// Journal what the actor's own recovery did during its turn, and follow it to the route it
/// ended on.
async fn journal_recovery(env: &Arc<Env>, alias: &str) -> Result<(), ConduitError> {
    let actor = {
        let actors = env.actors.lock().await;
        actors.get(alias).map(|runtime| runtime.actor.clone())
    };
    let Some(actor) = actor else {
        return Ok(());
    };
    for note in env.host.drain_recovery(&actor).await {
        let turn = env
            .actors
            .lock()
            .await
            .get(alias)
            .map_or(0, |runtime| runtime.turns);
        match note {
            RecoveryNote::Retry {
                attempt,
                max_attempts,
                wait_ms,
                class,
                detail,
            } => {
                env.emit(
                    alias,
                    turn,
                    WireOp::RecoveryRetry,
                    json!({
                        "attempt": attempt, "of": max_attempts, "delay_ms": wait_ms,
                        "class": class, "error": detail,
                    }),
                )
                .await?;
            }
            RecoveryNote::RouteChanged {
                from,
                to_agent,
                to_model,
                class,
                step,
            } => {
                let moved = ActorRef {
                    agent: to_agent,
                    model: to_model,
                    ..actor.clone()
                };
                let to = actor_name(&moved);
                {
                    let mut actors = env.actors.lock().await;
                    if let Some(runtime) = actors.get_mut(alias) {
                        runtime.fallbacks.push(to.clone());
                        runtime.actor = moved;
                    }
                }
                env.emit(
                    alias,
                    turn,
                    WireOp::RouteChanged,
                    json!({"from": from, "to": to, "class": class, "step": step}),
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// The actor `alias`, started on first use inside the scope's world.
pub(crate) async fn ensure_actor(
    env: &Arc<Env>,
    scope: &Scope,
    alias: &str,
) -> Result<ActorRef, ConduitError> {
    let actors = env.actors.lock().await;
    if let Some(runtime) = actors.get(alias) {
        return Ok(runtime.actor.clone());
    }
    let declared =
        env.program
            .actors
            .get(alias)
            .cloned()
            .ok_or_else(|| ConduitError::UnknownActor {
                alias: alias.to_owned(),
                known: env
                    .program
                    .actors
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
    // An actor that names a world lives in it, wherever the first message comes from.
    drop(actors);
    let world_name = declared.world.clone().or_else(|| scope.world.clone());
    let world = match &world_name {
        Some(name) => Some(super::control::open_world(env, name).await?.0),
        None => None,
    };
    let mut actors = env.actors.lock().await;
    if let Some(runtime) = actors.get(alias) {
        return Ok(runtime.actor.clone());
    }
    let mut sandbox = scope.sandbox.clone();
    for policy in world
        .iter()
        .map(|world| &world.sandbox)
        .chain(declared.sandbox.iter())
    {
        sandbox = Some(narrow_sandbox(sandbox.as_ref(), policy));
    }
    let mut tools = declared.tools.clone();
    let mut toolset_instructions = Vec::new();
    for toolset in env
        .program
        .toolsets
        .iter()
        .filter(|toolset| declared.toolsets.contains(&toolset.name))
    {
        tools.extend(toolset.allowed_tools.iter().cloned());
        toolset_instructions.extend(toolset.instructions.iter().cloned());
    }
    tools.sort();
    tools.dedup();
    let actor = env
        .host
        .spawn_actor(&ActorSpec {
            alias: alias.to_owned(),
            actor: declared,
            tools,
            toolset_instructions,
            sandbox,
            world,
        })
        .await?;
    env.emit(
        alias,
        0,
        WireOp::ActorSpawned,
        json!({"session": actor.session, "agent": actor.agent, "model": actor.model}),
    )
    .await?;
    actors.insert(
        alias.to_owned(),
        ActorRuntime {
            actor: actor.clone(),
            turns: 0,
            fallbacks: Vec::new(),
        },
    );
    Ok(actor)
}

/// Resolve a send target to a running actor.
pub(crate) async fn resolve_target(
    env: &Arc<Env>,
    scope: &Scope,
    target: &Target,
) -> Result<(String, ActorRef), ConduitError> {
    match target {
        Target::Declared(alias) => Ok((alias.clone(), ensure_actor(env, scope, alias).await?)),
        Target::External(reference) => {
            let actor = env.host.resolve_actor(reference).await?;
            let alias = actor.alias.clone();
            env.actors
                .lock()
                .await
                .entry(alias.clone())
                .or_insert_with(|| ActorRuntime {
                    actor: actor.clone(),
                    turns: 0,
                    fallbacks: Vec::new(),
                });
            Ok((alias, actor))
        }
    }
}

/// Hand `message` to the actor as a new turn and journal it.
async fn send_once(
    env: &Arc<Env>,
    alias: &str,
    message: &str,
    delivery: Delivery,
    recovery: &RecoveryOverride,
) -> Result<Receipt, ConduitError> {
    let actor = {
        let actors = env.actors.lock().await;
        actors
            .get(alias)
            .map(|runtime| runtime.actor.clone())
            .ok_or_else(|| ConduitError::UnknownActor {
                alias: alias.to_owned(),
                known: actors.keys().cloned().collect::<Vec<_>>().join(", "),
            })?
    };
    let receipt = env.host.send(&actor, message, delivery, recovery).await?;
    let turn = {
        let mut actors = env.actors.lock().await;
        let runtime = actors.get_mut(alias).ok_or_else(|| ConduitError::Runtime {
            message: format!("actor `{alias}` disappeared while a message was sent"),
        })?;
        runtime.turns += 1;
        runtime.turns
    };
    env.emit(
        alias,
        turn,
        WireOp::MessageSent,
        json!({"receipt": receipt.id, "text": message, "delivery": delivery}),
    )
    .await?;
    Ok(receipt)
}

/// Send (if the turn has not been sent yet) and wait for the reply. A failure here is one the
/// actor's own recovery could not fix.
pub(crate) async fn complete_turn(
    env: &Arc<Env>,
    scope: &Scope,
    pending: &mut PendingTurn,
    sent: bool,
) -> Result<Reply, ConduitError> {
    let alias = pending.alias.clone();
    let mut cancel = env.cancel.clone();
    let receipt = if sent {
        pending.receipt.clone()
    } else {
        let mut recovery = recovery_for(env, &alias, scope)?;
        recovery.merge(pending.recovery.clone());
        send_once(env, &alias, &pending.message, pending.delivery, &recovery).await?
    };
    pending.receipt = receipt.clone();
    let mut guard = InFlight {
        host: Arc::clone(&env.host),
        receipt: Some(receipt.clone()),
    };
    let outcome = tokio::select! {
        reply = env.host.await_reply(&receipt, None) => reply,
        _ = async {
            if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
                std::future::pending::<()>().await;
            }
        } => Err(ConduitError::Cancelled),
    };
    if outcome.is_ok() {
        guard.receipt = None;
    }
    drop(guard);
    journal_recovery(env, &alias).await?;
    let reply = outcome?;
    env.charge_tokens(scope.budget.as_ref(), reply.tokens)?;
    let turn = env
        .actors
        .lock()
        .await
        .get(&alias)
        .map_or(0, |runtime| runtime.turns);
    env.emit(
        &alias,
        turn,
        WireOp::MessageReplied,
        json!({
            "receipt": receipt.id, "text": reply.text,
            "tokens": reply.tokens, "tokens_estimated": reply.tokens_estimated,
        }),
    )
    .await?;
    Ok(reply)
}

/// Deliver a message from the program to an actor. With `wait` the value is the reply text;
/// without it, a receipt the program can `Await`.
pub(crate) async fn deliver(
    env: &Arc<Env>,
    scope: &mut Scope,
    target: &Target,
    message: &str,
    wait: bool,
    delivery: Delivery,
    receipt_name: Option<&str>,
) -> Result<crate::value::ConduitValue, ConduitError> {
    let (alias, _) = resolve_target(env, scope, target).await?;
    let recovery = recovery_for(env, &alias, scope)?;
    // The receipt is filled in by the first send; `complete_turn` sends when `sent` is false.
    let mut pending = PendingTurn {
        alias: alias.clone(),
        receipt: Receipt {
            actor: alias,
            turn: 0,
            id: String::new(),
        },
        message: message.to_owned(),
        delivery,
        recovery: recovery.clone(),
    };
    if wait {
        let reply = complete_turn(env, scope, &mut pending, false).await?;
        return Ok(crate::value::ConduitValue::String(reply.text));
    }
    let receipt = send_once(env, &pending.alias, message, delivery, &recovery).await?;
    let value = crate::value::ConduitValue::String(receipt.id.clone());
    pending.receipt = receipt;
    if let Some(name) = receipt_name {
        scope.receipts.insert(name.to_owned(), pending);
    }
    Ok(value)
}
