//! Committing a candidate agent once its session has started, off the event loop.
//!
//! When the agent a person switched to has started its session, a few round trips stand between
//! it and taking over: proving the conversation it was handed is still the conversation (a
//! journal read), putting the chosen model and effort on it (the agent's answers), and moving the
//! conversation's binding to it (journal writes). Awaited in the event handler, they froze the
//! whole interface for as long as the agent took to answer. They run on a task instead
//! ([`run`]), and the answer comes back as [`AppEvent::CandidateCommitted`], where the swap
//! itself, which is quick and touches only the app's own state, happens.
//!
//! Meanwhile the interface goes on, so the conversation can change under the switch: the outcome
//! is checked against the session as it stands when it arrives, and a switch whose conversation
//! moved on is dropped with one line, as before.

use std::sync::Arc;

use nori_protocol::SessionStarted;
use nori_protocol::acp::v1::SessionConfigOption;

use super::*;
use crate::chatwidget::HarnessHandle;

/// How committing a candidate went.
#[derive(Debug)]
pub(crate) enum CommitOutcome {
    /// Everything is in place: take the candidate, with these options when the model or effort
    /// was set on it.
    Ready(Option<Vec<SessionConfigOption>>),
    /// The conversation moved on while the candidate started: its context is stale.
    ConversationChanged,
    /// The candidate `agent` would not run `model`. `eligible`: another route may.
    ModelRefused {
        agent: String,
        model: String,
        eligible: bool,
        error: String,
    },
    /// The candidate would not keep the effort the conversation had.
    EffortRefused(String),
    /// The conversation's binding could not be moved to the candidate.
    BindingFailed,
    /// The previous agent's recorder could not be retired.
    RetireFailed,
}

/// What a commit needs, taken from the app when the candidate's session started.
pub(super) struct CommitPlan {
    candidate: Option<HarnessHandle>,
    /// The session being replaced, whose recorder a handoff retires.
    previous: Option<HarnessHandle>,
    config_options: Vec<SessionConfigOption>,
    /// The agent and model chosen for the candidate.
    model: Option<(String, String)>,
    effort: Option<String>,
    /// For a handoff: the conversation the candidate was handed, to prove it unchanged.
    handoff: Option<HandedOver>,
}

struct HandedOver {
    recorder: Arc<nori_harness::transcript::TranscriptRecorder>,
    head: Option<i64>,
    context: String,
}

async fn run(plan: CommitPlan) -> CommitOutcome {
    if let Some(handed) = &plan.handoff {
        // The journal head is the cheap proof of an unchanged conversation; only a moved head
        // (any event moves it) is worth reading the conversation again.
        let unchanged = handed.recorder.conversation_head().await == handed.head
            || handed.recorder.visible_context().await.as_deref().ok()
                == Some(handed.context.as_str());
        if !unchanged {
            return CommitOutcome::ConversationChanged;
        }
    }
    let mut options = None;
    if let Some((agent, model)) = &plan.model {
        let config_id = super::events::candidate_model_config(&plan.config_options, model);
        let chosen = match (plan.candidate.as_ref(), config_id) {
            (Some(handle), Some(config_id)) => handle
                .set_session_config_option(config_id, model.clone())
                .await
                .map_err(|error| {
                    (
                        crate::app::model_routes::eligible_error(&error),
                        error.to_string(),
                    )
                }),
            _ => Err((true, format!("{agent} no longer advertises model {model}"))),
        };
        let chosen = match chosen {
            Ok(chosen) => chosen,
            Err((eligible, error)) => {
                return CommitOutcome::ModelRefused {
                    agent: agent.clone(),
                    model: model.clone(),
                    eligible,
                    error,
                };
            }
        };
        let effort_option = chosen
            .iter()
            .find(|option| crate::nori::agent_config_state::is_effort_option(option));
        options = Some(
            match (&plan.effort, plan.candidate.as_ref(), effort_option) {
                (Some(effort), Some(handle), Some(option)) => match handle
                    .set_session_config_option(option.id.to_string(), effort.clone())
                    .await
                {
                    Ok(options) => options,
                    Err(error) => return CommitOutcome::EffortRefused(error.to_string()),
                },
                // No effort to keep, or an agent that offers no choice of it.
                _ => chosen,
            },
        );
    }
    if plan.handoff.is_some() {
        let bound = match &plan.candidate {
            Some(handle) => handle.record_backend_bound().await,
            None => Err(anyhow::anyhow!("Fallback candidate has no active session")),
        };
        if let Err(error) = bound {
            tracing::warn!(%error, "Failed to record fallback backend binding");
            return CommitOutcome::BindingFailed;
        }
        if let Some(previous) = &plan.previous
            && let Err(error) = previous.retire_handoff().await
        {
            tracing::warn!(%error, "Failed to retire previous backend recorder");
            if let Err(error) = previous.record_backend_bound().await {
                tracing::error!(%error, "Failed to restore the original backend binding");
            }
            return CommitOutcome::RetireFailed;
        }
    }
    CommitOutcome::Ready(options)
}

impl App {
    /// The candidate's session started: start committing it, or drop it at once when its
    /// conversation already moved on.
    pub(super) fn begin_candidate_commit(
        &mut self,
        tui: &mut tui::Tui,
        generation: crate::app_event::SessionGeneration,
        started: SessionStarted,
    ) {
        if let Some(handoff) = &self.manual_model_handoff
            && (self.chat_widget.session_generation() != handoff.source_generation
                || self.chat_widget.is_task_running())
        {
            self.drop_stale_handoff(tui);
            return;
        }
        let candidate = match &self.candidate_agent {
            Some(CandidateAgent::Activating { widget, .. }) => widget.harness_handle(),
            _ => None,
        };
        let plan = CommitPlan {
            candidate,
            previous: self.chat_widget.harness_handle(),
            config_options: started.config_options.clone(),
            model: self.pending_candidate_model.take(),
            effort: self
                .manual_model_handoff
                .as_ref()
                .and_then(|handoff| handoff.effort.clone()),
            handoff: self
                .manual_model_handoff
                .as_ref()
                .map(|handoff| HandedOver {
                    recorder: Arc::clone(&handoff.recorder),
                    head: handoff.head,
                    context: handoff.context.clone(),
                }),
        };
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let outcome = run(plan).await;
            tx.send(AppEvent::CandidateCommitted {
                generation,
                started: Box::new(started),
                outcome,
            });
        });
    }

    fn drop_stale_handoff(&mut self, tui: &mut tui::Tui) {
        self.discard_candidate();
        self.manual_model_handoff = None;
        self.model_route_fallback = None;
        tui.show_toast("Conversation changed; choose the model again".to_owned());
    }

    /// The commit of the candidate of `generation` ended with `outcome`: take the candidate, or
    /// say why not and try the next route of the model choice.
    pub(super) async fn finish_candidate_commit(
        &mut self,
        tui: &mut tui::Tui,
        generation: crate::app_event::SessionGeneration,
        started: SessionStarted,
        outcome: CommitOutcome,
    ) {
        let current = matches!(
            &self.candidate_agent,
            Some(CandidateAgent::Activating { widget, .. })
                if widget.session_generation() == generation
        );
        if !current {
            // The candidate ended or was replaced while it was being committed.
            return;
        }
        if let Some(handoff) = &self.manual_model_handoff
            && (self.chat_widget.session_generation() != handoff.source_generation
                || self.chat_widget.is_task_running())
        {
            self.drop_stale_handoff(tui);
            return;
        }
        let options = match outcome {
            CommitOutcome::Ready(options) => options,
            CommitOutcome::ConversationChanged => {
                self.drop_stale_handoff(tui);
                return;
            }
            CommitOutcome::EffortRefused(error) => {
                self.discard_candidate();
                let why = format!("it couldn't keep the effort setting ({error})");
                if !self.advance_model_route(&why) {
                    self.manual_model_handoff = None;
                }
                return;
            }
            CommitOutcome::ModelRefused {
                agent,
                model,
                eligible,
                error,
            } => {
                self.discard_candidate();
                let model_selection = self.model_route_fallback.is_some();
                if eligible && self.advance_model_route(&error) {
                    return;
                }
                self.end_model_choice(&error);
                if model_selection {
                    self.manual_model_handoff = None;
                    return;
                }
                self.chat_widget.add_error_message(format!(
                    "Couldn't select model {model} for {agent}: {error}"
                ));
                return;
            }
            CommitOutcome::BindingFailed => {
                self.discard_candidate();
                self.manual_model_handoff = None;
                self.model_route_fallback = None;
                tui.show_toast("Couldn't save the model switch".to_owned());
                return;
            }
            CommitOutcome::RetireFailed => {
                self.discard_candidate();
                self.manual_model_handoff = None;
                self.model_route_fallback = None;
                tui.show_toast("Couldn't finish the model switch".to_owned());
                return;
            }
        };
        let Some(CandidateAgent::Activating {
            agent_name, widget, ..
        }) = self.candidate_agent.take()
        else {
            return;
        };
        let mut widget = widget;
        if let Some(options) = &options {
            widget.sync_acp_session_config_snapshot(options);
        }
        self.model_route_fallback = None;
        let manual_handoff = self.manual_model_handoff.take().is_some();
        if manual_handoff {
            self.chat_widget.flush_active_cell();
        }
        let mut old_widget = std::mem::replace(&mut self.chat_widget, *widget);
        let draft = manual_handoff.then(|| old_widget.take_composer_draft());
        self.session_card_shown = true;
        self.config.active_agent = agent_name.clone();
        self.config.agent = agent_name;
        self.configure_new_chat_widget();
        if let Some(draft) = draft {
            self.chat_widget.restore_composer_draft(draft);
        }
        self.program_status
            .event(&nori_protocol::SessionEvent::Nori(
                nori_protocol::NoriEvent::SessionStarted(started.clone()),
            ));
        if let Some(handle) = self.chat_widget.harness_handle()
            && let Err(error) = self
                .remote_control
                .attach_started(handle, self.config.nori_home.clone(), started)
                .await
        {
            tracing::warn!(%error, "failed to attach committed candidate to remote ACP host");
        }
        if !manual_handoff {
            let (initial_prompt, initial_images) = old_widget.take_initial_input();
            self.chat_widget
                .submit_launch_input(initial_prompt, initial_images);
        }
        self.cancel_primary_agent_preparation();
        if let Some(agent) = self.prepared_agent.take() {
            tokio::spawn(agent.shutdown());
        }
        self.prepared_agent_initial_context = None;
        old_widget.shutdown_harness_session();
        self.save_config(
            "the agent selection",
            ConfigEditsBuilder::new(&self.config.nori_home).set_agent(&self.config.active_agent),
        );
        self.chat_widget.show_agent_in_footer();
        crate::latency::settle(crate::latency::Interaction::AgentSwitch);
    }
}
