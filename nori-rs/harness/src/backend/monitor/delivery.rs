//! The backend's side of monitors: tell the client how many are armed, and hand the agent its
//! events through the turn ledger when it is idle.
//!
//! The ledger is the only arbiter, exactly as for task results and peer mail: a batch is offered,
//! and starts a turn only if no turn of any owner is open at that instant. If it is refused the
//! events go back where they were and are offered again at the next chance. Nothing is queued
//! behind another turn, where it could outlive its reason.

use nori_protocol::MonitorWire;
use nori_protocol::MonitorsDelivered;
use nori_protocol::NoriEvent;
use nori_protocol::SessionEvent;
use nori_protocol::acp::v1 as acp;

use super::journal;
use super::journal::Record;
use super::registry::Effect;
use super::spec::Action;
use super::text;
use crate::backend::AcpBackend;
use crate::backend::BackendEvent;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

impl AcpBackend {
    /// Journal the session's monitors, publish how many are armed, and deliver their events as
    /// they come due, until the session ends.
    pub(in crate::backend) fn serve_monitors(&self) {
        let backend = self.clone();
        let monitors = self.orchestrator.monitors().clone();
        monitors.attach_journal(self.transcript_recorder_cell());
        let jobs = self.orchestrator.jobs().clone();
        jobs.attach_journal(self.transcript_recorder_cell());
        let (effects_tx, mut effects) = tokio::sync::mpsc::unbounded_channel::<Effect>();
        monitors.attach_effects(effects_tx);
        let performer = self.clone();
        tokio::spawn(async move {
            while let Some(effect) = effects.recv().await {
                performer.perform_monitor_action(effect).await;
            }
        });
        tokio::spawn(async move {
            let mut published = nori_protocol::MonitorsState::default();
            loop {
                let due = async {
                    match monitors.next_ready() {
                        Some(at) => tokio::time::sleep_until(at.into()).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = monitors.changed() => {}
                    () = due => {}
                }
                let mut state = monitors.summary();
                let counts = jobs.summary();
                state.jobs = counts.running + counts.unreachable;
                if state != published {
                    published = state.clone();
                    let _ = backend
                        .backend_event_tx
                        .send(BackendEvent::Public(SessionEvent::Nori(
                            NoriEvent::Monitor(MonitorWire::State(state)),
                        )))
                        .await;
                }
                if monitors.is_closed() {
                    break;
                }
                backend.deliver_monitor_events_if_idle(false).await;
            }
        });
    }

    /// Tell the person: one transcript line and an OS notification. The line is headed by `title`,
    /// or by `label` when there is none, and the notification can block, so it is never sent from
    /// the event loop. A monitor's `notify` action and a plugin's `notify` operation both do this.
    /// Returns the headline.
    pub(crate) async fn tell_person(
        &self,
        label: &str,
        title: Option<String>,
        body: String,
        failure: bool,
    ) -> String {
        let headline = title.as_deref().unwrap_or(label).to_owned();
        let message = match (failure, body.is_empty()) {
            (true, _) => format!("◉ {headline} FAILED: {body}"),
            (false, true) => format!("◉ {headline}"),
            (false, false) => format!("◉ {headline}: {body}"),
        };
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(NoriEvent::Notice(
                nori_protocol::Notice { message },
            ))))
            .await;
        let notifier = self.user_notifier.clone();
        let notification = crate::UserNotification::Monitor {
            label: label.to_owned(),
            title,
            body,
        };
        tokio::task::spawn_blocking(move || notifier.notify(&notification));
        headline
    }

    /// Do what a monitor's action asks, other than a wake (which goes through the queue).
    async fn perform_monitor_action(&self, effect: Effect) {
        match &effect.action {
            Action::Wake { .. } => {}
            Action::Notify { title, body } => {
                let body = effect.render(body.as_deref().unwrap_or("{{event}}"));
                let title = title.as_deref().map(|title| effect.render(title));
                self.tell_person(&effect.label, title, body, effect.failure)
                    .await;
            }
            Action::RunJob { job } => {
                let backend = self.clone();
                let job = job.clone();
                tokio::spawn(async move {
                    let jobs = backend.orchestrator.jobs();
                    // A job of that name already running is what the action asks for.
                    if matches!(
                        jobs.follow(&job),
                        Ok(crate::backend::jobs::Followed::Live { .. })
                    ) {
                        return;
                    }
                    let modules = backend.orchestrator.modules().await;
                    let started = match crate::backend::jobs::JobStartRequest::module(&job)
                        .into_start(&modules, &backend.cwd)
                    {
                        Ok(start) => jobs.start(start).await,
                        Err(error) => Err(error),
                    };
                    if let Err(error) = started {
                        backend
                            .notice(&format!(
                                "◉ {}: could not run job `{job}`: {error}",
                                effect.label
                            ))
                            .await;
                    }
                });
            }
            Action::StopJob { job } => {
                let backend = self.clone();
                let job = job.clone();
                tokio::spawn(async move {
                    // A job that is not running is already what the action asks for.
                    if let Err(error) = backend
                        .orchestrator
                        .jobs()
                        .stop(&job, None, std::time::Duration::from_secs(5))
                        .await
                    {
                        tracing::debug!(monitor = %effect.monitor, %job, %error, "a monitor's stop_job found nothing to stop");
                    }
                });
            }
            Action::RunSkill { skill, arguments } => {
                let backend = self.clone();
                let skill = skill.clone();
                let arguments = arguments
                    .iter()
                    .map(|(name, value)| (name.clone(), effect.render(value)))
                    .collect();
                tokio::spawn(async move {
                    let (text, failure) =
                        match backend.orchestrator.execute_skill(&skill, arguments).await {
                            Ok(result) => (
                                format!(
                                    "skill {skill} finished: {} (journal: {})",
                                    match &result.value {
                                        serde_json::Value::String(text) => text.clone(),
                                        other => other.to_string(),
                                    },
                                    result.journal
                                ),
                                false,
                            ),
                            Err(error) => (format!("skill {skill} failed: {error:#}"), true),
                        };
                    backend
                        .orchestrator
                        .monitors()
                        .post_outcome(&effect.monitor, text, failure)
                        .await;
                });
            }
            Action::Prompt { text } => {
                let text = effect.render(text);
                let backend = self.clone();
                // Admission can wait for a running turn; the next action must not wait with it.
                tokio::spawn(async move {
                    let (response_tx, response_rx) = tokio::sync::oneshot::channel();
                    backend
                        .submit_prompt(
                            vec![acp::ContentBlock::Text(acp::TextContent::new(text))],
                            None,
                            crate::backend::PromptAdmission::Queue,
                            response_tx,
                        )
                        .await;
                    if let Ok(Err(error)) = response_rx.await {
                        tracing::warn!(monitor = %effect.monitor, %error, "a monitor's prompt was not queued");
                    }
                });
            }
        }
    }

    /// Let this session's `session` monitors read its public events from now on.
    pub(crate) fn publish_events_to_monitors(
        &self,
        events: crate::session_event_fanout::SessionEventFanout,
    ) {
        self.orchestrator.monitors().attach_events(events);
    }

    /// A turn of any owner ended: events that were waiting for a boundary may come now.
    pub(in crate::backend) fn monitor_turn_boundary(&self) {
        let at_boundary = self.orchestrator.monitors().turn_boundary();
        let backend = self.clone();
        // Offering waits for the runtime queue, so it must not happen inside the runtime actor.
        tokio::spawn(async move { backend.deliver_monitor_events_if_idle(at_boundary).await });
    }

    /// Hand the agent the events that are ready, in a hidden turn, if it is idle. A busy agent
    /// gets them when its turn ends.
    pub(in crate::backend) async fn deliver_monitor_events_if_idle(&self, at_boundary: bool) {
        let _admission = self.orchestrator.rewind_admission().await;
        if self
            .is_shutting_down
            .load(std::sync::atomic::Ordering::Relaxed)
            || self.session_driver.lock().await.prompt_is_busy()
        {
            return;
        }
        let monitors = self.orchestrator.monitors();
        let Some(lease) = monitors.lease(at_boundary) else {
            return;
        };
        let body = text::wake_text(&lease);
        monitors
            .write(&Record::Batch {
                batch: lease.batch.clone(),
                seqs: lease.events.iter().map(|event| event.seq).collect(),
            })
            .await;
        let sent = self
            .offer_prompt(
                None,
                QueuedPrompt {
                    event_id: format!("monitor-events-{}", lease.batch),
                    kind: QueuedPromptKind::MonitorEvents,
                    text: body.clone(),
                    prompt_meta: None,
                    original_content: Vec::new(),
                    content: vec![acp::ContentBlock::Text(acp::TextContent::new(body))],
                    context: Default::default(),
                    display_text: None,
                },
            )
            .await;
        if !sent {
            monitors.release(lease);
            return;
        }
        monitors.commit();
        monitors
            .write(&Record::Delivered {
                batch: lease.batch.clone(),
            })
            .await;
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                NoriEvent::Monitor(MonitorWire::Delivered(MonitorsDelivered {
                    monitors: text::digests(&lease),
                })),
            )))
            .await;
    }

    /// Take back what the journal says: monitors armed before the last run come back dormant,
    /// events nobody had been handed are queued again, and the jobs that were running are adopted
    /// (after the monitors: a job's wake is the dormant monitor restored here).
    pub(in crate::backend) async fn restore_monitors(&self) {
        self.restore_monitor_records().await;
        self.restore_jobs().await;
    }

    async fn restore_monitor_records(&self) {
        let Some(recorder) = self.transcript_recorder.read().await.clone() else {
            return;
        };
        let records: Vec<Record> = match recorder.journaled_commands(journal::COMMAND).await {
            Ok(payloads) => payloads
                .iter()
                .filter_map(|payload| serde_json::from_str(payload).ok())
                .collect(),
            Err(error) => {
                tracing::warn!(%error, "failed to read the journaled monitors");
                return;
            }
        };
        if records.is_empty() {
            return;
        }
        let texts = recorder
            .visible_records_where(|kind, _| kind == nori_conversation::Kind::Message)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|record| match record.event {
                nori_conversation::Event::Message { content, .. } => Some(content),
                _ => None,
            });
        let in_conversation = journal::batches_in(texts);
        self.orchestrator
            .monitors()
            .restore(journal::fold(records, &in_conversation));
    }
}
