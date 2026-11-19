//! The queue of typed-ahead prompts, journaled so a session that ends with prompts still queued
//! gives them back when it is resumed.
//!
//! Each change to the queue is one small `queue` command in the conversation journal:
//! `enqueue` (at the back, or at the front when steering), `start`, and `remove`. Folding them
//! in order yields what was queued when the journal ended. Journals without these commands
//! fold to nothing, so older transcripts load as before.

use nori_protocol::acp::v1 as acp;
use serde::Deserialize;
use serde::Serialize;

use super::session_reducer::QueueOp;
use crate::normalized::session_runtime::QueuedPrompt;
use crate::normalized::session_runtime::QueuedPromptKind;

pub(crate) const QUEUE_COMMAND: &str = "queue";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct QueuedPromptRecord {
    pub(crate) event_id: String,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) display_text: Option<String>,
    #[serde(default)]
    pub(crate) prompt_meta: Option<acp::Meta>,
    #[serde(default)]
    pub(crate) original_content: Vec<acp::ContentBlock>,
    #[serde(default)]
    pub(crate) content: Vec<acp::ContentBlock>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(crate) enum QueueRecord {
    Enqueue {
        #[serde(flatten)]
        prompt: QueuedPromptRecord,
        #[serde(default)]
        front: bool,
    },
    Start {
        event_id: String,
    },
    Remove {
        event_id: String,
    },
}

/// The journal record of a queue change. Only prompts the user typed are journaled; hidden
/// continuations are decided afresh by the session that resumes.
pub(crate) fn record_of(op: &QueueOp) -> Option<QueueRecord> {
    match op {
        QueueOp::Enqueued { prompt, front } if prompt.kind == QueuedPromptKind::User => {
            Some(QueueRecord::Enqueue {
                prompt: QueuedPromptRecord {
                    event_id: prompt.event_id.clone(),
                    text: prompt.text.clone(),
                    display_text: prompt.display_text.clone(),
                    prompt_meta: prompt.prompt_meta.clone(),
                    original_content: prompt.original_content.clone(),
                    content: prompt.content.clone(),
                },
                front: *front,
            })
        }
        QueueOp::Enqueued { .. } => None,
        QueueOp::Started { event_id } => Some(QueueRecord::Start {
            event_id: event_id.clone(),
        }),
        QueueOp::Removed { event_id } => Some(QueueRecord::Remove {
            event_id: event_id.clone(),
        }),
    }
}

/// What was queued when the journal ended, oldest first.
pub(crate) fn fold(records: impl IntoIterator<Item = QueueRecord>) -> Vec<QueuedPrompt> {
    let mut queue: std::collections::VecDeque<QueuedPromptRecord> = Default::default();
    for record in records {
        match record {
            QueueRecord::Enqueue {
                prompt,
                front: true,
            } => queue.push_front(prompt),
            QueueRecord::Enqueue {
                prompt,
                front: false,
            } => queue.push_back(prompt),
            QueueRecord::Start { event_id } | QueueRecord::Remove { event_id } => {
                queue.retain(|prompt| prompt.event_id != event_id);
            }
        }
    }
    queue
        .into_iter()
        .map(|record| QueuedPrompt {
            event_id: record.event_id,
            kind: QueuedPromptKind::User,
            text: record.text,
            prompt_meta: record.prompt_meta,
            original_content: record.original_content,
            content: record.content,
            display_text: record.display_text,
        })
        .collect()
}

impl super::AcpBackend {
    /// Journal the queue changes the ledger just made, before any effect of them runs.
    pub(super) async fn journal_queue_ops(&self, ops: &[QueueOp]) {
        if ops.is_empty() {
            return;
        }
        let Some(recorder) = self.transcript_recorder.read().await.clone() else {
            return;
        };
        for record in ops.iter().filter_map(record_of) {
            let Ok(payload) = serde_json::to_string(&record) else {
                continue;
            };
            if let Err(error) = recorder.record_command(QUEUE_COMMAND, payload).await {
                tracing::warn!(%error, "failed to journal a queue change");
            }
        }
    }

    /// Give back, as held prompts, what the journal says was queued when the previous session
    /// ended.
    pub(super) async fn restore_queued_prompts(&self) {
        let Some(recorder) = self.transcript_recorder.read().await.clone() else {
            return;
        };
        let prompts = match recorder.journaled_queue().await {
            Ok(prompts) => prompts,
            Err(error) => {
                tracing::warn!(%error, "failed to read the journaled queue");
                return;
            }
        };
        if !prompts.is_empty() {
            self.apply_session_event_boxed(super::session_reducer::InboundEvent::RestoreQueued(
                prompts,
            ))
            .await;
        }
    }

    /// The user dropped a queued or restored prompt.
    pub(crate) async fn drop_queued(&self, event_id: String) -> anyhow::Result<()> {
        self.session_event_tx
            .send(super::session_runtime_driver::SessionRuntimeInput::Reducer(
                super::session_reducer::InboundEvent::DropQueued { event_id },
            ))
            .await
            .map_err(|_| anyhow::anyhow!("session runtime closed"))
    }
}
