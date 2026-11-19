//! Moving a session to another route: in place on its own agent, or by asking the runtime to
//! replace the agent. Also moving back to the route the user chose once it can be used again.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;

use nori_protocol::NoriEvent;
use nori_protocol::acp::v1 as acp;
use nori_workflow::ReturnToPrimary;
use nori_workflow::Route;
use nori_workflow::RouteCause;
use nori_workflow::RouteRef;
use tokio::sync::mpsc;
use tokio::sync::watch;

use super::*;
use crate::recovery::handoff::HandoffRequest;
use crate::recovery::ledger;
use crate::recovery::ledger::Recheck;

/// Per-session recovery state that outlives one prompt: the retry cancel switch, the channel
/// to the runtime that can replace the agent, and what a replacement agent needs to start.
#[derive(Clone)]
pub(super) struct SessionRecovery {
    cancel: watch::Sender<i64>,
    handoff: Arc<StdMutex<Option<mpsc::UnboundedSender<HandoffRequest>>>>,
    session_context: Option<SessionContext>,
}

impl SessionRecovery {
    pub(super) fn new(session_context: Option<SessionContext>) -> Self {
        Self {
            cancel: watch::channel(0).0,
            handoff: Arc::default(),
            session_context,
        }
    }

    /// The user cancelled: stop waiting to retry.
    pub(super) fn cancel(&self) {
        self.cancel.send_modify(|count| *count += 1);
    }

    pub(super) fn cancelled(&self) -> watch::Receiver<i64> {
        self.cancel.subscribe()
    }
}

/// When a route may be restored.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestoreAt {
    /// Before a prompt starts a turn.
    TurnBoundary,
    /// A new session of the conversation has just started.
    SessionStart,
}

/// How long after the first re-check of an unavailable model the second one comes.
const SECOND_RECHECK_SECONDS: i64 = 60 * 60;

/// What a replacement agent is built from.
pub(crate) struct HandoffTemplate {
    pub(crate) config: Arc<crate::config::NoriConfig>,
    pub(crate) cli_version: String,
    pub(crate) session_context: Option<SessionContext>,
    pub(crate) seed: super::orchestrator::Seed,
    pub(crate) recorder: Option<Arc<TranscriptRecorder>>,
}

pub(super) fn model_option(
    options: &[acp::SessionConfigOption],
) -> Option<&acp::SessionConfigOption> {
    options.iter().find(|option| {
        option.category == Some(acp::SessionConfigOptionCategory::Model)
            || option.id.to_string() == "model"
    })
}

/// A select option's current value and every `(value, name)` it offers.
pub(super) fn select_values(
    option: &acp::SessionConfigOption,
) -> Option<(String, Vec<(String, String)>)> {
    let acp::SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    let offered = match &select.options {
        acp::SessionConfigSelectOptions::Ungrouped(options) => options.iter().collect(),
        acp::SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter())
            .collect(),
        _ => Vec::new(),
    };
    Some((
        select.current_value.to_string(),
        offered
            .into_iter()
            .map(|option| (option.value.to_string(), option.name.clone()))
            .collect(),
    ))
}

impl AcpBackend {
    /// The runtime that owns this backend will replace it when asked to.
    pub(crate) fn bind_handoff(&self, requests: mpsc::UnboundedSender<HandoffRequest>) {
        *self
            .recovery
            .handoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(requests);
    }

    /// Whether a route on another agent can be used: something is bound to replace the agent.
    pub(crate) fn can_hand_off(&self) -> bool {
        self.recovery
            .handoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Ask the runtime to continue the conversation on another agent. False when nothing is
    /// bound to do it.
    pub(crate) fn request_handoff(&self, request: HandoffRequest) -> bool {
        self.recovery
            .handoff
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|requests| requests.send(request).is_ok())
    }

    /// Everything a replacement agent needs to start where this one stands.
    pub(crate) async fn handoff_template(&self) -> HandoffTemplate {
        HandoffTemplate {
            config: Arc::clone(self.orchestrator.config()),
            cli_version: self.cli_version.clone(),
            session_context: self.recovery.session_context.clone(),
            seed: self.orchestrator.seed().clone(),
            recorder: self.transcript_recorder.read().await.clone(),
        }
    }

    /// The cheap check that a model which failed as unavailable works again: the agent is asked
    /// what it offers now, and the model must still be on that list.
    async fn reprobe_confirms(&self, route: &RouteRef) -> bool {
        let Some(model) = &route.model else {
            return false;
        };
        let reprobed = super::orchestrator::reprobe(
            Arc::clone(self.orchestrator.config()),
            self.cli_version.clone(),
            &route.agent,
        )
        .await;
        if reprobed.is_err()
            || !super::orchestrator::advertises(&self.nori_home, &route.agent, model)
        {
            return false;
        }
        super::orchestrator::clear_unavailable(&self.nori_home, &route.agent, model);
        true
    }

    /// Whether no turn is open, so the session may change route before the next one.
    pub(crate) async fn is_between_turns(&self) -> bool {
        matches!(
            self.session_driver.lock().await.public_phase(),
            nori_protocol::SessionPhase::Idle
        )
    }

    /// The route this session runs on now: its agent and the model its model option selects.
    pub(crate) fn current_route(&self) -> RouteRef {
        let options = self.connection.config_options();
        let model = model_option(&options)
            .and_then(select_values)
            .map(|(current, _)| current);
        RouteRef::new(self.agent_name.clone(), model)
    }

    /// Select `route`'s model (and settings) on this agent's own session.
    pub(crate) async fn switch_to_route(&self, route: &Route) -> anyhow::Result<RouteRef> {
        let session_id = self.session_id.read().await.clone();
        let options = self.connection.config_options();
        let Some(wanted) = route.model.as_deref() else {
            anyhow::bail!("the route names no model");
        };
        let option = model_option(&options)
            .ok_or_else(|| anyhow::anyhow!("this agent does not let Nori choose its model"))?;
        let (current, offered) = select_values(option)
            .ok_or_else(|| anyhow::anyhow!("this agent's model option is not a list"))?;
        let value = offered
            .iter()
            .find(|(value, _)| value == wanted)
            .or_else(|| {
                offered.iter().find(|(value, name)| {
                    value.eq_ignore_ascii_case(wanted) || name.eq_ignore_ascii_case(wanted)
                })
            })
            .map(|(value, _)| value.clone())
            .ok_or_else(|| anyhow::anyhow!("this agent does not offer a model `{wanted}`"))?;
        if value != current {
            self.connection
                .set_config_option(&session_id, option.id.clone(), value.clone())
                .await?;
        }
        for (setting, wanted) in &route.options {
            let Some(option) = options
                .iter()
                .find(|option| option.id.to_string() == *setting)
            else {
                continue;
            };
            let Some((_, values)) = select_values(option) else {
                continue;
            };
            if let Some((value, _)) = values
                .iter()
                .find(|(value, name)| value == wanted || name.eq_ignore_ascii_case(wanted))
                && let Err(error) = self
                    .connection
                    .set_config_option(&session_id, option.id.clone(), value.clone())
                    .await
            {
                warn!(%error, %setting, "route could not set an option");
            }
        }
        Ok(RouteRef::new(self.agent_name.clone(), Some(value)))
    }

    /// Move this session to `to` on its own agent and say so: the new options for whatever
    /// shows the model, then the route change.
    pub(crate) async fn switch_in_place(
        &self,
        to: &Route,
        from: &RouteRef,
        cause: RouteCause,
        step: nori_protocol::FallbackStep,
        detail: Option<String>,
    ) -> anyhow::Result<RouteRef> {
        let route = self.switch_to_route(to).await?;
        let accounts = crate::recovery::AccountHeadroom::load(&self.nori_home);
        let session_id = self.session_id.read().await.clone();
        self.wait_for_prompt_phase().await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Acp(
                nori_protocol::AcpEvent::Notification(acp::AgentNotification::SessionNotification(
                    acp::SessionNotification::new(
                        session_id,
                        acp::SessionUpdate::ConfigOptionUpdate(acp::ConfigOptionUpdate::new(
                            self.connection.config_options(),
                        )),
                    ),
                )),
            )))
            .await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::RouteChanged(nori_protocol::RouteChanged {
                    from: from.clone(),
                    to: route.clone(),
                    cause,
                    step,
                    detail,
                    from_account: Some(accounts.describe(from)),
                    to_account: Some(accounts.describe(&route)),
                }),
            )))
            .await;
        Ok(route)
    }

    /// Go back to the route the user chose, if a fallback left it and the policy says it may be
    /// used again. A route on this agent is selected in place; a route on another agent comes
    /// back as a request for the caller to hand to the runtime.
    pub(crate) async fn restore_primary(&self, at: RestoreAt) -> Option<HandoffRequest> {
        let conversation = self.conversation_id.read().await.to_string();
        let primary = ledger::primary(&conversation)?;
        let current = self.current_route();
        if current == primary.route {
            ledger::clear_primary(&conversation);
            return None;
        }
        let due = match (primary.policy, at) {
            (ReturnToPrimary::AfterReset, _) => match primary.restore_at {
                Some(restore_at) => ledger::now_seconds() >= restore_at,
                // No time is known: wait for a new session, as `next_session` does.
                None => at == RestoreAt::SessionStart,
            },
            (ReturnToPrimary::NextSession, RestoreAt::SessionStart) => true,
            (ReturnToPrimary::NextSession, RestoreAt::TurnBoundary)
            | (ReturnToPrimary::Never, _) => false,
        };
        if !due {
            return None;
        }
        if let Recheck::Reprobe { probes } = primary.recheck {
            // The model failed as unavailable: ask the agent what it offers now (15 minutes
            // after the failure, then an hour later, then no more) before trusting it again.
            if !self.reprobe_confirms(&primary.route).await {
                let next = (probes == 0).then(|| ledger::now_seconds() + SECOND_RECHECK_SECONDS);
                ledger::reschedule(&conversation, next, Recheck::Reprobe { probes: probes + 1 });
                return None;
            }
            ledger::pardon(&conversation, &primary.route);
        }
        if ledger::blocks(&conversation, &primary.route) {
            return None;
        }
        ledger::forgive(&conversation, &primary.route);
        let route = Route {
            agent: Some(primary.route.agent.clone()),
            model: primary.route.model.clone(),
            ..Route::default()
        };
        if primary.route.agent == self.agent_name {
            let step = nori_protocol::FallbackStep { index: 1, total: 1 };
            match self
                .switch_in_place(&route, &current, RouteCause::PrimaryRestored, step, None)
                .await
            {
                Ok(_) => ledger::clear_primary(&conversation),
                Err(error) => warn!(%error, "could not go back to the primary route"),
            }
            return None;
        }
        if !self.can_hand_off() {
            return None;
        }
        ledger::clear_primary(&conversation);
        Some(HandoffRequest {
            from: current,
            cause: RouteCause::PrimaryRestored,
            plan: vec![route],
            prompt: None,
            detail: None,
        })
    }
}
