//! Evaluate op-graph nodes: the step wrapper (journal, timeout, cancellation, recovery scope)
//! and the effects that act on one thing at a time. Compound steps are in [`super::control`].

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use super::actors;
use super::control;
use super::env::Env;
use super::env::Scope;
use crate::error::ConduitError;
use crate::op::Node;
use crate::op::Op;
use crate::op::ToolRef;
use crate::policy::enforce_sandbox_tool;
use crate::program::template;
use crate::schemas::ToolSource;
use crate::snapshot::RunStatus;
use crate::tools::ToolCall;
use crate::value::ConduitValue;
use crate::wire::WireOp;

pub(crate) type EvalFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ConduitValue, ConduitError>> + Send + 'a>>;

impl Scope {
    /// Fill a `{{name}}` template from the bindings in scope.
    pub(crate) fn render(&self, text: &str) -> Result<String, ConduitError> {
        template::render(text, &|name| {
            self.bindings.get(name).map(ConduitValue::display_compact)
        })
        .map_err(|missing| ConduitError::UnboundName {
            bound: self.bindings.keys().cloned().collect::<Vec<_>>().join(", "),
            name: missing,
        })
    }

    fn render_json(&self, value: &serde_json::Value) -> Result<serde_json::Value, ConduitError> {
        Ok(match value {
            serde_json::Value::String(text) => serde_json::Value::String(self.render(text)?),
            serde_json::Value::Array(items) => serde_json::Value::Array(
                items
                    .iter()
                    .map(|item| self.render_json(item))
                    .collect::<Result<_, _>>()?,
            ),
            serde_json::Value::Object(map) => serde_json::Value::Object(
                map.iter()
                    .map(|(key, item)| Ok((key.clone(), self.render_json(item)?)))
                    .collect::<Result<_, ConduitError>>()?,
            ),
            other => other.clone(),
        })
    }

    pub(crate) fn bind(&mut self, env: &Env, name: Option<&str>, value: &ConduitValue) {
        if let Some(name) = name {
            self.bindings.insert(name.to_owned(), value.clone());
            env.note_binding(name, value);
        }
    }
}

/// Run one node: journal its start and end, honour its timeout and the run's cancellation,
/// and scope its recovery override to the node.
pub(crate) fn eval_node<'a>(
    env: &'a Arc<Env>,
    node: &'a Node,
    scope: &'a mut Scope,
) -> EvalFuture<'a> {
    Box::pin(async move {
        env.emit_run(
            WireOp::StepStarted,
            json!({"path": format!("{}{}", node.path, scope.tag), "kind": node.op.kind(), "id": node.id}),
        )
        .await?;
        let outer_recovery = scope.recovery.clone();
        if let Some(block) = &node.recovery {
            let over = block
                .to_override()
                .map_err(|message| ConduitError::Runtime {
                    message: format!("recovery at {}: {message}", node.path),
                })?;
            scope.recovery.merge(over);
        }
        let mut cancel = env.cancel.clone();
        let body = eval_op(env, node, scope);
        let body = async {
            match node.timeout_ms {
                Some(ms) => tokio::time::timeout(
                    Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0)),
                    body,
                )
                .await
                .unwrap_or(Err(ConduitError::Timeout { ms })),
                None => body.await,
            }
        };
        let result = tokio::select! {
            result = body => result,
            _ = async {
                if cancel.wait_for(|cancelled| *cancelled).await.is_err() {
                    std::future::pending::<()>().await;
                }
            } => Err(ConduitError::Cancelled),
        };
        scope.recovery = outer_recovery;
        match &result {
            Ok(value) => {
                scope.last = value.clone();
                scope.bind(env, node.bind.as_deref(), value);
                env.emit_run(
                    WireOp::StepFinished,
                    json!({"path": format!("{}{}", node.path, scope.tag), "value": value.to_plain_json()}),
                )
                .await?;
            }
            Err(error) => {
                env.emit_run(
                    WireOp::StepFailed,
                    json!({
                        "path": format!("{}{}", node.path, scope.tag), "error": error.to_string(),
                        "class": error.failure_kind().map(nori_workflow::FailureKind::name),
                    }),
                )
                .await?;
            }
        }
        env.write_snapshot(RunStatus::Running, None, None).await?;
        result
    })
}

pub(crate) async fn eval_seq(
    env: &Arc<Env>,
    nodes: &[Node],
    scope: &mut Scope,
) -> Result<ConduitValue, ConduitError> {
    let mut last = ConduitValue::Unit;
    for node in nodes {
        last = eval_node(env, node, scope).await?;
    }
    Ok(last)
}

fn eval_op<'a>(env: &'a Arc<Env>, node: &'a Node, scope: &'a mut Scope) -> EvalFuture<'a> {
    Box::pin(async move {
        match &node.op {
            Op::Prompt { text } => {
                env.host.prompt(&scope.render(text)?).await?;
                Ok(ConduitValue::Unit)
            }
            Op::Config { key, value } => {
                env.host.set_config(key, &scope.render(value)?).await?;
                Ok(ConduitValue::Unit)
            }
            Op::Spawn { actor } => {
                let started = actors::ensure_actor(env, scope, actor).await?;
                Ok(ConduitValue::String(started.session))
            }
            Op::Send {
                to,
                message,
                wait,
                delivery,
                receipt,
            } => {
                let text = scope.render(message)?;
                actors::deliver(env, scope, to, &text, *wait, *delivery, receipt.as_deref()).await
            }
            Op::Await { receipt } => {
                let mut pending =
                    scope
                        .receipts
                        .remove(receipt)
                        .ok_or_else(|| ConduitError::UnboundName {
                            name: receipt.clone(),
                            bound: scope
                                .receipts
                                .keys()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", "),
                        })?;
                let reply = actors::complete_turn(env, scope, &mut pending, true).await?;
                Ok(ConduitValue::String(reply.text))
            }
            Op::Call { tool, args } => {
                let value = ConduitValue::from_json(scope.render_json(args)?);
                let (name, kind) = match tool {
                    ToolRef::Host { name } => (name.as_str(), "host"),
                    ToolRef::Declared { tool } => match tool.source {
                        ToolSource::Host => (tool.name.as_str(), "host"),
                        ToolSource::Wasm { .. } => (tool.name.as_str(), "wasm"),
                    },
                };
                enforce_sandbox_tool(scope.sandbox.as_ref(), name)?;
                let outcome = match tool {
                    ToolRef::Declared { tool } if kind == "wasm" => {
                        env.tools.check(tool)?;
                        env.tools
                            .invoke(ToolCall {
                                tool,
                                args: &value,
                                base_dir: &env.base_dir,
                            })
                            .await?
                    }
                    ToolRef::Host { .. } | ToolRef::Declared { .. } => {
                        env.host
                            .call_tool(name, &value, scope.sandbox.as_ref())
                            .await?
                    }
                };
                record_tool(env, scope, name, kind, &value, &outcome).await?;
                Ok(outcome.output)
            }
            Op::Exec { program, args, cwd } => {
                enforce_sandbox_tool(scope.sandbox.as_ref(), "unsafe")?;
                let rendered: Vec<String> = args
                    .iter()
                    .map(|arg| scope.render(arg))
                    .collect::<Result<_, _>>()?;
                let outcome = env.host.exec(program, &rendered, cwd.as_deref()).await?;
                let input = ConduitValue::from_json(json!({"program": program, "args": rendered}));
                record_tool(env, scope, program, "process", &input, &outcome).await?;
                if outcome.output.to_plain_json().get("success")
                    == Some(&serde_json::Value::Bool(false))
                {
                    return Err(ConduitError::ToolFailure {
                        name: program.clone(),
                        reason: outcome.output.display_compact(),
                    });
                }
                Ok(outcome.output)
            }
            Op::SelectContext { start, end } => Ok(ConduitValue::String(
                env.host.select_context(*start, *end).await?,
            )),
            Op::Fork { source, start, end } => {
                env.host.fork(source, *start, *end).await?;
                Ok(ConduitValue::Unit)
            }
            Op::Rewind { head } => {
                env.host.rewind(*head).await?;
                Ok(ConduitValue::Unit)
            }
            Op::Emit { topic, payload } => {
                let payload = ConduitValue::String(scope.render(payload)?);
                env.signals.emit(topic, payload.clone());
                env.emit_run(
                    WireOp::Signal,
                    json!({"topic": topic, "payload": payload.to_plain_json()}),
                )
                .await?;
                Ok(ConduitValue::Unit)
            }
            Op::AwaitSignal { topic } => Ok(env.signals.await_topic(topic).await),
            Op::Interrupt { prompt } => {
                let prompt = scope.render(prompt)?;
                control::interrupt(env, &format!("{}{}", node.path, scope.tag), &prompt).await
            }
            Op::Delay { ms } => {
                tokio::time::sleep(Duration::from_millis(
                    u64::try_from((*ms).max(0)).unwrap_or(0),
                ))
                .await;
                Ok(ConduitValue::Unit)
            }
            Op::Pin { fact } => {
                {
                    let mut state = env.lock_state();
                    if !state.pins.contains(fact) {
                        state.pins.push(fact.clone());
                    }
                }
                env.emit_run(WireOp::Note, json!({"pin": fact})).await?;
                Ok(ConduitValue::String(fact.clone()))
            }
            Op::Checkpoint { name } => {
                env.lock_state().checkpoints.push(name.clone());
                env.emit_run(WireOp::Note, json!({"checkpoint": name}))
                    .await?;
                Ok(ConduitValue::String(name.clone()))
            }
            Op::Seq { body } => eval_seq(env, body, scope).await,
            Op::Par { branches } => control::par(env, branches, scope).await,
            Op::Race { branches } => control::race(env, branches, scope).await,
            Op::FanOut { over, each, body } => control::fan_out(env, over, each, body, scope).await,
            Op::Loop {
                max_iterations,
                until,
                body,
            } => control::repeat(env, *max_iterations, until.as_ref(), body, scope).await,
            Op::When {
                check,
                then,
                otherwise,
            } => {
                if control::holds(check, scope, None)? {
                    eval_seq(env, then, scope).await
                } else {
                    eval_seq(env, otherwise, scope).await
                }
            }
            Op::Guard {
                phase,
                check,
                feedback,
                max_attempts,
                on_exhausted,
                body,
            } => {
                control::guard(
                    env,
                    *phase,
                    check,
                    feedback,
                    *max_attempts,
                    *on_exhausted,
                    body,
                    scope,
                )
                .await
            }
            Op::Budget { tokens, body } => {
                let outer = scope.budget.clone();
                scope.budget = Some(crate::policy::BudgetFrame::child(outer.clone(), *tokens));
                let result = eval_seq(env, body, scope).await;
                scope.budget = outer;
                result
            }
            Op::Within { world, body } => control::within(env, world, body, scope).await,
            Op::Retry { body } => control::retry(env, node, body, scope).await,
            Op::Recover { body, fallback } => {
                let mut attempt = scope.clone();
                match eval_seq(env, body, &mut attempt).await {
                    Ok(value) => {
                        *scope = attempt;
                        Ok(value)
                    }
                    Err(ConduitError::Cancelled) => Err(ConduitError::Cancelled),
                    Err(error) => {
                        env.emit_run(
                            WireOp::Note,
                            json!({"recovered": node.path, "error": error.to_string()}),
                        )
                        .await?;
                        eval_seq(env, fallback, scope).await
                    }
                }
            }
        }
    })
}

async fn record_tool(
    env: &Env,
    scope: &Scope,
    name: &str,
    kind: &str,
    args: &ConduitValue,
    outcome: &crate::host::ToolOutcome,
) -> Result<(), ConduitError> {
    env.charge_tokens(scope.budget.as_ref(), outcome.tokens)?;
    env.lock_state().tool_calls += 1;
    env.emit_run(
        WireOp::ToolCalled,
        json!({
            "tool": name, "kind": kind, "args": args.to_plain_json(),
            "output": outcome.output.to_plain_json(), "tokens": outcome.tokens,
        }),
    )
    .await
}
