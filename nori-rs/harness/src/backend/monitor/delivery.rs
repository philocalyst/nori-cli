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
                let state = monitors.summary();
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
    /// and events nobody had been handed are queued again.
    pub(in crate::backend) async fn restore_monitors(&self) {
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
            .visible_records()
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
