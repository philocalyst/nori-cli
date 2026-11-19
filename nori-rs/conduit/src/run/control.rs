//! Compound steps: concurrency, loops, guards, worlds, retry and interrupts.

use std::sync::Arc;
use std::time::Duration;

use futures::future::select_ok;
use futures::future::try_join_all;
use serde_json::json;

use super::env::Env;
use super::env::Scope;
use super::eval::eval_node;
use super::eval::eval_seq;
use crate::error::ConduitError;
use crate::host::WorldScope;
use crate::op::Node;
use crate::policy::BudgetFrame;
use crate::policy::narrow_sandbox;
use crate::program::model::Check;
use crate::program::model::GuardPhase;
use crate::program::model::OnExhausted;
use crate::program::model::World;
use crate::schemas::Isolation;
use crate::schemas::SandboxPolicy;
use crate::value::ConduitValue;
use crate::wire::WireOp;

/// Whether `check` holds. `output` supplies the binding of that name (a post-guard's result).
pub(crate) fn holds(
    check: &Check,
    scope: &Scope,
    output: Option<&ConduitValue>,
) -> Result<bool, ConduitError> {
    let value = match (check.value.as_str(), output) {
        ("output", Some(output)) => output,
        (name, _) => scope
            .bindings
            .get(name)
            .ok_or_else(|| ConduitError::UnboundName {
                name: name.to_owned(),
                bound: scope
                    .bindings
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", "),
            })?,
    };
    let text = value.display_compact();
    let verdict = match (&check.contains, &check.equals, &check.starts_with) {
        (Some(needle), _, _) => text.contains(needle.as_str()),
        (None, Some(expected), _) => text.trim() == expected.trim(),
        (None, None, Some(prefix)) => text.starts_with(prefix.as_str()),
        (None, None, None) => false,
    };
    Ok(verdict != check.negate)
}

/// Run every branch at once with its own copy of the scope; the first failure cancels the
/// rest. Bindings the branches made are visible afterwards.
pub(crate) async fn par(
    env: &Arc<Env>,
    branches: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let runs = branches.iter().map(|node| {
        let mut branch = scope.clone();
        async move {
            let value = eval_node(env, node, &mut branch).await?;
            Ok::<_, ConduitError>((value, branch))
        }
    });
    let finished = try_join_all(runs).await?;
    // Only what a branch changed is merged: a branch that did not touch a name still
    // carries the value it started with, which must not overwrite another branch's.
    let before = scope.bindings.clone();
    let mut values = Vec::with_capacity(finished.len());
    for (value, branch) in finished {
        for (name, bound) in branch.bindings {
            if before.get(&name) != Some(&bound) {
                scope.bindings.insert(name, bound);
            }
        }
        for (name, pending) in branch.receipts {
            scope.receipts.entry(name).or_insert(pending);
        }
        values.push(value);
    }
    Ok(ConduitValue::List(values))
}

/// Run every branch at once; the first success wins and the others are cancelled.
pub(crate) async fn race(
    env: &Arc<Env>,
    branches: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let runs: Vec<_> = branches
        .iter()
        .map(|node| {
            let mut branch = scope.clone();
            Box::pin(async move {
                let value = eval_node(env, node, &mut branch).await?;
                Ok::<_, ConduitError>((value, branch))
            })
        })
        .collect();
    match select_ok(runs).await {
        Ok(((value, winner), _losers)) => {
            scope.bindings.extend(winner.bindings);
            scope.receipts.extend(winner.receipts);
            Ok(value)
        }
        Err(last) => Err(ConduitError::Runtime {
            message: format!(
                "all {} branches of the race failed; the last said: {last}",
                branches.len()
            ),
        }),
    }
}

/// Run `body` once per item, all at once, binding the item as `each`.
pub(crate) async fn fan_out(
    env: &Arc<Env>,
    over: &[String],
    each: &str,
    body: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let items: Vec<String> = over
        .iter()
        .map(|item| scope.render(item))
        .collect::<Result<_, _>>()?;
    let runs = items.into_iter().map(|item| {
        let mut iteration = scope.clone();
        iteration.tag = format!("{}[{each}={item}]", scope.tag);
        iteration
            .bindings
            .insert(each.to_owned(), ConduitValue::String(item));
        async move { eval_seq(env, body, &mut iteration).await }
    });
    Ok(ConduitValue::List(try_join_all(runs).await?))
}

/// Repeat `body`; with `until`, stop when it holds and fail if it never does.
pub(crate) async fn repeat(
    env: &Arc<Env>,
    max_iterations: i64,
    until: Option<&Check>,
    body: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let mut last = ConduitValue::Unit;
    for _ in 0..max_iterations {
        last = eval_seq(env, body, scope).await?;
        if let Some(check) = until
            && holds(check, scope, None)?
        {
            return Ok(last);
        }
    }
    match until {
        Some(_) => Err(ConduitError::LoopExhausted { max_iterations }),
        None => Ok(last),
    }
}

/// Check before or after `body`; while a post-check fails run it again with `feedback` bound.
#[expect(clippy::too_many_arguments)]
pub(crate) async fn guard(
    env: &Arc<Env>,
    phase: GuardPhase,
    check: &Check,
    feedback: &str,
    max_attempts: i64,
    on_exhausted: OnExhausted,
    body: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let mut attempts = 0;
    match phase {
        GuardPhase::Pre => {
            attempts = 1;
            let passed = holds(check, scope, None)?;
            env.emit_run(
                WireOp::Note,
                json!({"guard": "pre", "passed": passed, "feedback": feedback}),
            )
            .await?;
            if passed {
                return eval_seq(env, body, scope).await;
            }
        }
        GuardPhase::Post => {
            for attempt in 1..=max_attempts {
                attempts = attempt;
                let said = if attempt == 1 { "" } else { feedback };
                scope
                    .bindings
                    .insert("feedback".to_owned(), ConduitValue::String(said.to_owned()));
                let output = eval_seq(env, body, scope).await?;
                let passed = holds(check, scope, Some(&output))?;
                env.emit_run(
                    WireOp::Note,
                    json!({"guard": "post", "attempt": attempt, "passed": passed, "feedback": feedback}),
                )
                .await?;
                if passed {
                    scope.bindings.remove("feedback");
                    return Ok(output);
                }
            }
            scope.bindings.remove("feedback");
        }
    }
    match on_exhausted {
        OnExhausted::Fail => Err(ConduitError::GuardRejected {
            phase: phase.as_str().to_owned(),
            feedback: feedback.to_owned(),
            attempts,
        }),
        OnExhausted::Skip => Ok(ConduitValue::Null),
    }
}

/// Open the world `name` (once): create its directories and tell the host. Returns the world,
/// its definition, and whether this call opened it.
pub(crate) async fn open_world(
    env: &Arc<Env>,
    name: &str,
) -> Result<(WorldScope, World, bool), ConduitError> {
    let definition = env
        .program
        .worlds
        .get(name)
        .or(env.program.world.as_ref().filter(|_| name == "program"))
        .cloned()
        .ok_or_else(|| ConduitError::Runtime {
            message: format!("world `{name}` is not declared"),
        })?;
    let mut worlds = env.worlds.lock().await;
    if let Some(open) = worlds.get(name) {
        return Ok((open.clone(), definition, false));
    }
    let base = env
        .run_dir
        .clone()
        .unwrap_or_else(|| {
            env.host
                .workspace_root()
                .join(".nori")
                .join("runs")
                .join(&env.run)
        })
        .join("worlds")
        .join(name);
    let root = match definition.isolation {
        Isolation::Shared => env.host.workspace_root().to_path_buf(),
        Isolation::Worktree | Isolation::Sandbox => base.join("tree"),
    };
    let home = definition.home.then(|| base.join("home"));
    if let Some(home) = &home {
        std::fs::create_dir_all(home).map_err(|error| ConduitError::Runtime {
            message: format!("cannot create the home of world `{name}`: {error}"),
        })?;
    }
    let world = WorldScope {
        name: name.to_owned(),
        isolation: definition.isolation,
        root,
        home,
        sandbox: SandboxPolicy {
            allowed_tools: definition.allowed_tools.clone(),
            allowed_paths: definition.allowed_paths.clone(),
        },
        budget_tokens: definition.budget_tokens,
    };
    env.host.enter_world(&world).await?;
    worlds.insert(name.to_owned(), world.clone());
    drop(worlds);
    env.emit_run(
        WireOp::WorldOpened,
        json!({
            "world": name, "isolation": definition.isolation,
            "root": world.root, "home": world.home,
        }),
    )
    .await?;
    Ok((world, definition, true))
}

pub(crate) async fn close_world(env: &Arc<Env>, world: &WorldScope) -> Result<(), ConduitError> {
    env.worlds.lock().await.remove(&world.name);
    env.host.leave_world(world).await?;
    env.emit_run(WireOp::WorldClosed, json!({"world": world.name}))
        .await
}

/// Open a world, run `body` inside it with its narrower sandbox and budget, then close it.
pub(crate) async fn within(
    env: &Arc<Env>,
    name: &str,
    body: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let (world, definition, opened) = open_world(env, name).await?;
    let outer = (
        scope.sandbox.clone(),
        scope.budget.clone(),
        scope.world.clone(),
    );
    scope.sandbox = Some(narrow_sandbox(scope.sandbox.as_ref(), &world.sandbox));
    if let Some(tokens) = definition.budget_tokens {
        scope.budget = Some(BudgetFrame::child(scope.budget.clone(), tokens));
    }
    scope.world = Some(name.to_owned());
    let result = eval_seq(env, body, scope).await;
    (scope.sandbox, scope.budget, scope.world) = outer;
    if opened {
        close_world(env, &world).await?;
    }
    result
}

/// Run `body` again, as a whole, when it fails in a way the resolved `retry` policy retries.
pub(crate) async fn retry(
    env: &Arc<Env>,
    node: &Node,
    body: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let mut recovery = env.host.base_recovery("", None);
    if let Some(block) = &env.program.recovery {
        block
            .to_override()
            .map_err(|message| ConduitError::Runtime { message })?
            .apply(&mut recovery);
    }
    scope.recovery.apply(&mut recovery);
    let mut retries = 0;
    loop {
        let mut attempt = scope.clone();
        let error = match eval_seq(env, body, &mut attempt).await {
            Ok(value) => {
                *scope = attempt;
                return Ok(value);
            }
            Err(error) => error,
        };
        let retryable = error
            .failure_kind()
            .is_some_and(|kind| recovery.retry.on.contains(&kind));
        if !retryable || retries >= recovery.retry.max_attempts {
            return Err(error);
        }
        retries += 1;
        let delay = recovery.retry.backoff(retries, None);
        env.emit_run(
            WireOp::RecoveryRetry,
            json!({
                "step": node.path, "attempt": retries, "of": recovery.retry.max_attempts,
                "delay_ms": i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
                "class": error.failure_kind().map(nori_workflow::FailureKind::name),
                "error": error.to_string(),
            }),
        )
        .await?;
        tokio::time::sleep(delay.min(Duration::from_secs(3600))).await;
    }
}

/// Suspend until the interrupt `id` is answered, through the interrupt registry.
pub(crate) async fn interrupt(
    env: &Arc<Env>,
    id: &str,
    prompt: &str,
) -> Result<ConduitValue, ConduitError> {
    let answer = env.interrupts.register(id, prompt);
    env.lock_state().pending_interrupts = env.interrupts.snapshot_pending();
    env.emit_run(
        WireOp::InterruptRequested,
        json!({"interrupt": id, "prompt": prompt}),
    )
    .await?;
    let response = answer.await.map_err(|_| ConduitError::Runtime {
        message: format!("interrupt `{id}` was dropped before it was answered"),
    })?;
    env.lock_state().pending_interrupts = env.interrupts.snapshot_pending();
    Ok(ConduitValue::String(response))
}
