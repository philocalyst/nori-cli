//! Running a prompt under the session's recovery policy.
//!
//! A prompt that fails is classified and handed to the recovery [`Engine`]. A retry waits out a
//! visible, cancellable backoff on the same route. A fallback to another model of this agent is
//! applied in place and reported as `RouteChanged`. A fallback to another agent ends the run
//! with a [`HandoffRequest`]: the runtime replaces the agent and sends the prompt again there.

use std::time::Duration;

use nori_acp_host::classify_failure;
use nori_protocol::NoriEvent;
use nori_protocol::acp::v1 as acp;
use nori_workflow::Exclusion;
use nori_workflow::Recovery;
use nori_workflow::ReturnToPrimary;
use nori_workflow::RouteCause;
use nori_workflow::RouteRef;
use tokio::sync::oneshot;
use tokio::sync::watch;

use super::*;
use crate::normalized::session_runtime::QueuedPromptKind;
use crate::recovery::RouteRanking;
use crate::recovery::engine::Decision;
use crate::recovery::engine::Engine;
use crate::recovery::handoff::HandoffRequest;
use crate::recovery::installed_agents;
use crate::recovery::ledger;
use crate::recovery::ledger::Recheck;
use crate::recovery::plan;

/// How long an agent that could not run (not signed in, missing, failing to start) is left out
/// of a conversation's fallback routes.
const AGENT_UNRUNNABLE_FOR_SECONDS: i64 = 15 * 60;

pub(super) struct PromptRun {
    pub(super) wire_request_id: acp::RequestId,
    pub(super) result: anyhow::Result<acp::StopReason>,
    /// The route chosen is on another agent; the caller hands this to the runtime.
    pub(super) handoff: Option<HandoffRequest>,
}

/// When the agent states no reset time, how long `after_reset` waits before going back.
const UNKNOWN_RESET_SECONDS: i64 = 60 * 60;
/// How long after a model failed as unavailable the first re-check of it comes.
const FIRST_RECHECK_SECONDS: i64 = 15 * 60;

fn unit_random() -> f64 {
    use std::hash::BuildHasher;
    use std::hash::Hasher;
    let bits = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    (bits >> 11) as f64 / (1u64 << 53) as f64
}

impl AcpBackend {
    /// The recovery policy that applies to this session, once the workflow policy has loaded.
    async fn recovery_in_force(&self) -> Recovery {
        let mut ready = self.workflow_ready.subscribe();
        while !*ready.borrow_and_update() {
            if ready.changed().await.is_err() {
                break;
            }
        }
        let policy = self.workflow.lock().await.recovery.clone();
        let mut recovery = policy.resolve(&self.agent_name, self.orchestrator.role());
        // A program that started this actor lays its own overrides over the policy for the turn.
        if let Some(over) = self.orchestrator.recovery_override() {
            over.apply(&mut recovery);
        }
        recovery
    }

    async fn recovery_notice(&self, message: String) {
        self.wait_for_prompt_phase().await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                nori_protocol::Notice { message },
            ))))
            .await;
    }

    /// Send a prompt, recovering from failures as the policy says. Returns the last attempt's
    /// outcome; a failure that reaches the caller is one the policy could not recover.
    pub(super) async fn prompt_with_recovery(
        &self,
        session_id: acp::SessionId,
        mut prompt: Vec<acp::ContentBlock>,
        prompt_meta: Option<acp::Meta>,
        mut transport_started: Option<oneshot::Sender<anyhow::Result<acp::RequestId>>>,
        kind: QueuedPromptKind,
        turn_seq: Option<i64>,
    ) -> PromptRun {
        let mut cancelled = self.recovery.cancelled();
        let mut engine_slot: Option<Engine> = None;
        // A prompt handed over by a fallback runs on a route Nori chose, not the person: if that
        // route cannot run at all, the fallback goes on to the next one.
        let continues_fallback = prompt_meta
            .as_ref()
            .is_some_and(|meta| meta.contains_key(nori_protocol::HANDOFF_RETRY_META_KEY));
        let mut current = self.current_route();
        // The turn has one request ID on the public stream, the first attempt's, whatever the
        // later attempts are called on the wire. A prompt handed over from another route keeps
        // the ID it had there, so its client sees the request it sent end.
        let mut first_id: Option<acp::RequestId> = prompt_meta
            .as_ref()
            .and_then(|meta| meta.get(nori_protocol::HANDOFF_RETRY_META_KEY))
            .filter(|marker| !marker.is_boolean())
            .and_then(|marker| serde_json::from_value(marker.clone()).ok());
        if let Some(original) = first_id.clone()
            && let Some(started) = transport_started.take()
        {
            // The turn is keyed by the handed-over ID from its first attempt.
            let (forward_tx, forward_rx) = oneshot::channel();
            transport_started = Some(forward_tx);
            tokio::spawn(async move {
                if let Ok(result) = forward_rx.await {
                    let _ = started.send(result.map(|_| original));
                }
            });
        }
        let mut attempt_no: i64 = 0;
        // The prompt as first sent and the steps of `compaction.shrink_ladder` not yet tried,
        // set up the first time a provider refuses the prompt as too long.
        let mut ladder: Option<(Vec<acp::ContentBlock>, std::collections::VecDeque<f64>)> = None;
        loop {
            attempt_no += 1;
            let (wire_request_id, result) = self
                .connection
                .prompt_attempt(
                    session_id.clone(),
                    prompt.clone(),
                    prompt_meta.clone(),
                    transport_started.take(),
                    first_id.clone(),
                    false,
                )
                .await;
            let wire_request_id = first_id.get_or_insert(wire_request_id).clone();
            let err = match result {
                Ok(stop_reason) => {
                    return PromptRun {
                        wire_request_id,
                        result: Ok(stop_reason),
                        handoff: None,
                    };
                }
                Err(err) => err,
            };
            let details = classify_failure(&err);
            let failure = details.class.kind();
            // History Nori put into the prompt is the one part of it Nori may cut: a provider
            // that refuses the prompt as too long is sent the checkpoint cut to each step of the
            // policy's shrink ladder in turn, each a fraction of the checkpoint as first built.
            if failure == nori_workflow::FailureKind::PromptTooLong {
                if ladder.is_none() {
                    let steps = self.session_policy().await.compaction.shrink_ladder;
                    ladder = Some((prompt.clone(), steps.into()));
                }
                if let Some((original, steps)) = ladder.as_mut() {
                    let mut cut = None;
                    while let Some(fraction) = steps.pop_front() {
                        let mut smaller = original.clone();
                        if crate::context_budget::shrink_prompt_checkpoint(&mut smaller, fraction) {
                            cut = Some((smaller, fraction));
                            break;
                        }
                    }
                    if let Some((smaller, fraction)) = cut {
                        prompt = smaller;
                        self.supersede_attempt(
                            attempt_no,
                            turn_seq,
                            nori_protocol::SupersededBy::Retry,
                        )
                        .await;
                        self.recovery_notice(format!(
                            "The earlier conversation sent with this prompt was too long for the \
                             model; retrying with a shorter summary of it ({}% of its size).",
                            (fraction * 100.0).round()
                        ))
                        .await;
                        continue;
                    }
                }
            }
            let conversation = self.conversation_id.read().await.to_string();
            let policy = match engine_slot {
                Some(_) => None,
                None => Some(self.recovery_in_force().await),
            };
            let engine = engine_slot.get_or_insert_with(|| {
                let recovery =
                    policy.unwrap_or_else(|| nori_workflow::RecoveryPolicy::default().base);
                if continues_fallback {
                    Engine::continuing(recovery)
                } else {
                    Engine::new(recovery)
                }
            });
            self.note_failed_route(&conversation, &current, &details, engine.recovery())
                .await;
            let decision = {
                let excluded = ledger::exclusions(&conversation);
                let home = self.nori_home.clone();
                let config = self.orchestrator.config().clone();
                // How long until a spent usage limit resets, when the provider says.
                let reset_in = match &details.class {
                    nori_acp_host::FailureClass::QuotaExhausted {
                        resets_at: Some(resets_at),
                    } => ledger::parse_reset(resets_at)
                        .and_then(|at| u64::try_from(at - ledger::now_seconds()).ok())
                        .map(std::time::Duration::from_secs),
                    _ => None,
                };
                engine.decide(
                    failure,
                    details.class.retry_after(),
                    reset_in,
                    unit_random(),
                    kind == QueuedPromptKind::User,
                    |fallback| {
                        let catalog = crate::recovery::route_catalog(&home, Some(&self.agent_name));
                        let installed: Vec<String> = installed_agents(Some(&self.agent_name))
                            .into_iter()
                            .map(|info| info.agent_name)
                            .collect();
                        let mut ranking = RouteRanking::new(
                            Some(current.clone()),
                            &fallback.chain,
                            &|agent| config.default_models.get(agent).cloned(),
                            &installed,
                        )
                        .with_account_preferences(config.account_preferences.clone());
                        if fallback.prefer_headroom {
                            ranking = ranking
                                .with_headroom(crate::recovery::AccountHeadroom::load(&home));
                        }
                        plan::build(&plan::PlanInput {
                            policy: fallback,
                            current: &current,
                            catalog: &catalog,
                            ranking: &ranking,
                            excluded: &excluded,
                        })
                    },
                )
            };
            // The attempt that failed is over, whatever the policy does next: what it left
            // running must not keep showing as running through a backoff or on another route.
            let superseded_by = match &decision {
                Decision::Retry { .. } => Some(nori_protocol::SupersededBy::Retry),
                Decision::Fallback { .. } => Some(nori_protocol::SupersededBy::Route),
                Decision::Surface { .. } => None,
                Decision::WaitForReset { .. } => Some(nori_protocol::SupersededBy::Retry),
            };
            if let Some(by) = superseded_by {
                self.supersede_attempt(attempt_no, turn_seq, by).await;
            }
            match decision {
                Decision::Retry {
                    delay,
                    attempt,
                    max_attempts,
                } => {
                    self.wait_for_prompt_phase().await;
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            NoriEvent::RetryScheduled(nori_protocol::RetryScheduled {
                                class: failure,
                                attempt,
                                max_attempts,
                                wait_ms: i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
                                detail: details.message.clone(),
                            }),
                        )))
                        .await;
                    if wait_unless_cancelled(delay, &mut cancelled).await {
                        self.connection
                            .announce_prompt_stop(
                                wire_request_id.clone(),
                                acp::StopReason::Cancelled,
                            )
                            .await;
                        return PromptRun {
                            wire_request_id,
                            result: Ok(acp::StopReason::Cancelled),
                            handoff: None,
                        };
                    }
                }
                Decision::Fallback {
                    to,
                    step,
                    remaining,
                } => {
                    // Leaving the route the user was on: remember it, so the session can go
                    // back when the policy allows, and never offer it again meanwhile.
                    let return_to_primary = engine.recovery().fallback.return_to_primary;
                    let now = ledger::now_seconds();
                    // When `after_reset` may look at the primary route again, and how it
                    // finds out. Capacity has no cheap check, so it waits for a new session.
                    let (restore_at, recheck) = if return_to_primary == ReturnToPrimary::AfterReset
                    {
                        match &details.class {
                            nori_acp_host::FailureClass::QuotaExhausted { resets_at } => (
                                Some(
                                    resets_at
                                        .as_deref()
                                        .and_then(ledger::parse_reset)
                                        .unwrap_or(now + UNKNOWN_RESET_SECONDS),
                                ),
                                Recheck::None,
                            ),
                            nori_acp_host::FailureClass::RateLimited {
                                retry_after: Some(wait),
                            } => (
                                Some(now + i64::try_from(wait.as_secs()).unwrap_or(i64::MAX / 2)),
                                Recheck::None,
                            ),
                            nori_acp_host::FailureClass::ModelUnavailable => (
                                Some(now + FIRST_RECHECK_SECONDS),
                                Recheck::Reprobe { probes: 0 },
                            ),
                            _ => (None, Recheck::None),
                        }
                    } else {
                        (None, Recheck::None)
                    };
                    ledger::note_primary(
                        &conversation,
                        &current,
                        return_to_primary,
                        restore_at,
                        recheck,
                    );
                    ledger::exclude(&conversation, Exclusion::Route(current.clone()), None, true);
                    let mut next = Some((to, step, remaining));
                    let mut switched = false;
                    while let Some((to, step, remaining)) = next.take() {
                        let target = to.agent.clone().unwrap_or_else(|| self.agent_name.clone());
                        let unusable = if target == self.agent_name {
                            match self
                                .switch_in_place(
                                    &to,
                                    &current,
                                    RouteCause::Failure(failure),
                                    step,
                                    details.message.clone(),
                                )
                                .await
                            {
                                Ok(route) => {
                                    current = route;
                                    switched = true;
                                    break;
                                }
                                Err(skip) => skip.to_string(),
                            }
                        } else if self.can_hand_off() {
                            // Another agent: the runtime replaces this one and carries the
                            // conversation, and the prompt, over.
                            let mut plan = vec![to];
                            plan.extend(remaining);
                            return PromptRun {
                                wire_request_id,
                                result: Err(err),
                                handoff: Some(HandoffRequest {
                                    from: current.clone(),
                                    cause: RouteCause::Failure(failure),
                                    plan,
                                    prompt: None,
                                    detail: details.message.clone(),
                                }),
                            };
                        } else {
                            "nothing here can start another agent".to_owned()
                        };
                        self.recovery_notice(format!(
                            "Skipping {}: {unusable}.",
                            RouteRef::new(target, to.model.clone())
                        ))
                        .await;
                        next = match engine.next_route() {
                            Some(Decision::Fallback {
                                to,
                                step,
                                remaining,
                            }) => Some((to, step, remaining)),
                            Some(
                                Decision::Retry { .. }
                                | Decision::Surface { .. }
                                | Decision::WaitForReset { .. },
                            )
                            | None => None,
                        };
                    }
                    if !switched {
                        self.recovery_notice(format!(
                            "{} on {current}; no other route is available to continue on.",
                            capitalized(failure.label())
                        ))
                        .await;
                        return self.give_up(wire_request_id, err).await;
                    }
                }
                Decision::WaitForReset { delay } => {
                    let at = chrono::Local::now()
                        + chrono::Duration::from_std(delay)
                            .unwrap_or_else(|_| chrono::Duration::zero());
                    self.recovery_notice(format!(
                        "{} on {current}; this turn continues when it resets, at {} (Esc stops waiting).",
                        capitalized(failure.label()),
                        at.format("%a %H:%M")
                    ))
                    .await;
                    self.wait_for_prompt_phase().await;
                    let _ = self
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            NoriEvent::RetryScheduled(nori_protocol::RetryScheduled {
                                class: failure,
                                attempt: 1,
                                max_attempts: 0,
                                wait_ms: i64::try_from(delay.as_millis()).unwrap_or(i64::MAX),
                                detail: details.message.clone(),
                            }),
                        )))
                        .await;
                    if wait_unless_cancelled(delay, &mut cancelled).await {
                        self.connection
                            .announce_prompt_stop(
                                wire_request_id.clone(),
                                acp::StopReason::Cancelled,
                            )
                            .await;
                        return PromptRun {
                            wire_request_id,
                            result: Ok(acp::StopReason::Cancelled),
                            handoff: None,
                        };
                    }
                    // The limit has reset: the route is usable again for this conversation.
                    ledger::lift(&conversation, &current);
                }
                Decision::Surface { no_route_left } => {
                    if no_route_left {
                        self.recovery_notice(format!(
                            "{} on {current}; no other route is available to continue on.",
                            capitalized(failure.label())
                        ))
                        .await;
                    }
                    return self.give_up(wire_request_id, err).await;
                }
            }
        }
    }

    /// The attempt that failed is over: tell the stream and the turn, so that what it left
    /// running stops showing as running.
    async fn supersede_attempt(
        &self,
        attempt: i64,
        turn_seq: Option<i64>,
        by: nori_protocol::SupersededBy,
    ) {
        self.wait_for_prompt_phase().await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::AttemptSuperseded(nori_protocol::AttemptSuperseded { attempt, by }),
            )))
            .await;
        if let Some(turn_seq) = turn_seq {
            let _ = self
                .session_event_tx
                .send(session_runtime_driver::SessionRuntimeInput::Reducer(
                    session_reducer::InboundEvent::AttemptEnded { turn_seq },
                ))
                .await;
        }
    }

    /// The policy has nothing more to try: publish the failure as the turn's response, which
    /// the attempts did not, and hand it back.
    async fn give_up(&self, wire_request_id: acp::RequestId, err: anyhow::Error) -> PromptRun {
        self.connection
            .announce_prompt_failure(wire_request_id.clone(), &err)
            .await;
        PromptRun {
            wire_request_id,
            result: Err(err),
            handoff: None,
        }
    }

    /// Remember what a failure says about the route it happened on, whatever the policy does
    /// next: exhausted usage rules the provider out for the conversation, and an unavailable
    /// model is no longer offered as runnable.
    async fn note_failed_route(
        &self,
        conversation: &str,
        route: &RouteRef,
        details: &nori_acp_host::FailureDetails,
        recovery: &Recovery,
    ) {
        match &details.class {
            nori_acp_host::FailureClass::QuotaExhausted { resets_at } => {
                let lapses = (recovery.fallback.return_to_primary == ReturnToPrimary::AfterReset)
                    .then(|| resets_at.as_deref().and_then(ledger::parse_reset))
                    .flatten();
                ledger::exclude(
                    conversation,
                    Exclusion::exhausted(route, crate::recovery::is_gateway(&route.agent)),
                    lapses,
                    false,
                );
            }
            nori_acp_host::FailureClass::ModelUnavailable => {
                if let Some(model) = &route.model {
                    let reason = details
                        .detail
                        .as_deref()
                        .or(details.message.as_deref())
                        .map_or("the provider reports it unavailable", first_line);
                    super::orchestrator::mark_unavailable(
                        &self.nori_home,
                        &route.agent,
                        model,
                        reason,
                    );
                }
                ledger::exclude(conversation, Exclusion::Route(route.clone()), None, false);
            }
            // An agent that is not signed in, not installed or does not start cannot run any
            // of its models: none is offered as a fallback for a while (a sign-in brings it
            // back then).
            nori_acp_host::FailureClass::Auth
            | nori_acp_host::FailureClass::ExecutableNotFound
            | nori_acp_host::FailureClass::Initialization => {
                let lapses = chrono::Utc::now().timestamp() + AGENT_UNRUNNABLE_FOR_SECONDS;
                ledger::exclude(
                    conversation,
                    Exclusion::Provider {
                        agent: route.agent.clone(),
                        provider: None,
                    },
                    Some(lapses),
                    false,
                );
            }
            nori_acp_host::FailureClass::RateLimited { .. }
            | nori_acp_host::FailureClass::Capacity
            | nori_acp_host::FailureClass::ServerError
            | nori_acp_host::FailureClass::Network
            | nori_acp_host::FailureClass::PromptTooLong
            | nori_acp_host::FailureClass::SessionNotFound
            | nori_acp_host::FailureClass::SessionNotResumable
            | nori_acp_host::FailureClass::NoActiveSession
            | nori_acp_host::FailureClass::SessionAlreadyActive
            | nori_acp_host::FailureClass::Unknown => {}
        }
    }
}

async fn wait_unless_cancelled(delay: Duration, cancelled: &mut watch::Receiver<i64>) -> bool {
    if cancelled.has_changed().unwrap_or(true) {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(delay) => false,
        _ = cancelled.changed() => true,
    }
}

fn first_line(text: &str) -> &str {
    let line = text.lines().next().unwrap_or_default().trim();
    match line.char_indices().nth(100) {
        Some((end, _)) => &line[..end],
        None => line,
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}
