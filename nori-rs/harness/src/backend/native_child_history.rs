//! Child-specific journals fed from the owning provider connection.
use crate::transcript::SessionEventEntry;
use crate::transcript::TranscriptEntry;
use crate::transcript::TranscriptLoader;
use crate::transcript::TranscriptRecorder;
use anyhow::Context;
use anyhow::Result;
use nori_protocol::AcpEvent;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentState;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::RwLock;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct ReboundBinding {
    pub(super) project: String,
    pub(super) session: String,
    pub(super) parent: String,
    pub(super) agent: String,
    pub(super) model: String,
    pub(super) options: Vec<(String, String)>,
    pub(super) rebound: bool,
}

pub(super) struct NativeHistory {
    home: PathBuf,
    cwd: PathBuf,
    agent: String,
    version: String,
    parent: Arc<RwLock<Option<Arc<TranscriptRecorder>>>>,
    journals: Mutex<HashMap<acp::SessionId, Arc<TranscriptRecorder>>>,
    user_sources: Mutex<HashMap<(acp::SessionId, String), bool>>,
}

impl NativeHistory {
    pub(super) fn new(
        home: PathBuf,
        cwd: PathBuf,
        agent: String,
        version: String,
        parent: Arc<RwLock<Option<Arc<TranscriptRecorder>>>>,
    ) -> Self {
        Self {
            home,
            cwd,
            agent,
            version,
            parent,
            journals: Mutex::new(HashMap::new()),
            user_sources: Mutex::new(HashMap::new()),
        }
    }

    pub(super) async fn recorder(&self, id: &acp::SessionId) -> Result<Arc<TranscriptRecorder>> {
        let mut journals = self.journals.lock().await;
        if let Some(recorder) = journals.get(id) {
            return Ok(recorder.clone());
        }
        let parent = self
            .parent
            .read()
            .await
            .clone()
            .context("This child has no conversation journal")?;
        let key = format!("native-child-journal:{id}");
        let recorder = if let Some(payload) = parent.recorded_command_payload(&key).await {
            let binding: serde_json::Value = serde_json::from_str(&payload)?;
            let project = binding["project"]
                .as_str()
                .context("Invalid child journal project")?;
            let session = binding["session"]
                .as_str()
                .context("Invalid child journal identity")?;
            let loader = TranscriptLoader::new(self.home.clone());
            let transcript = loader.load_transcript(project, session).await?;
            Arc::new(
                TranscriptRecorder::continue_session(
                    loader.session_path(project, session),
                    &transcript.meta,
                )
                .await?,
            )
        } else {
            let recorder = Arc::new(
                TranscriptRecorder::new_delegated(
                    &self.home,
                    &self.cwd,
                    Some(self.agent.clone()),
                    &self.version,
                    Some(id.to_string()),
                )
                .await?,
            );
            persist_binding(&parent, &key, serde_json::json!({"project":recorder.project_id(), "session":recorder.session_id()}).to_string()).await?;
            recorder
        };
        journals.insert(id.clone(), recorder.clone());
        Ok(recorder)
    }

    pub(super) async fn rebound_binding(
        &self,
        id: &acp::SessionId,
    ) -> Result<Option<ReboundBinding>> {
        let parent = self
            .parent
            .read()
            .await
            .clone()
            .context("This child has no conversation journal")?;
        let key = format!("native-child-journal:{id}");
        let Some(payload) = parent.recorded_command_payload(&key).await else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_str(&payload)?;
        if value.get("rebound").and_then(serde_json::Value::as_bool) != Some(true) {
            return Ok(None);
        }
        Ok(Some(serde_json::from_value(value)?))
    }

    pub(super) async fn bind_rebound(
        &self,
        id: &acp::SessionId,
        recorder: Arc<TranscriptRecorder>,
        binding: ReboundBinding,
    ) -> Result<()> {
        let parent = self
            .parent
            .read()
            .await
            .clone()
            .context("This child has no conversation journal")?;
        persist_binding(
            &parent,
            &format!("native-child-journal:{id}"),
            serde_json::to_string(&binding)?,
        )
        .await?;
        self.journals.lock().await.insert(id.clone(), recorder);
        Ok(())
    }

    pub(super) async fn bind_native(
        &self,
        id: &acp::SessionId,
        recorder: Arc<TranscriptRecorder>,
    ) -> Result<()> {
        let parent = self
            .parent
            .read()
            .await
            .clone()
            .context("This child has no conversation journal")?;
        persist_binding(
            &parent,
            &format!("native-child-journal:{id}"),
            serde_json::json!({"project":recorder.project_id(), "session":recorder.session_id()})
                .to_string(),
        )
        .await?;
        self.journals.lock().await.insert(id.clone(), recorder);
        Ok(())
    }

    pub(super) async fn record_notification(
        &self,
        notification: &acp::SessionNotification,
    ) -> Result<()> {
        if let acp::SessionUpdate::UserMessageChunk(chunk) = &notification.update
            && let Some(message_id) = &chunk.message_id
        {
            let mut sources = self.user_sources.lock().await;
            let source = sources
                .entry((notification.session_id.clone(), message_id.to_string()))
                .or_insert(false);
            if *source {
                return Ok(());
            }
        }
        let recorder = self.recorder(&notification.session_id).await?;
        let event = SessionEvent::Acp(AcpEvent::Notification(
            acp::AgentNotification::SessionNotification(notification.clone()),
        ));
        recorder
            .record_session_event(&SessionEventEntry::new(event))
            .await?;
        Ok(())
    }

    pub(super) async fn record_wire(&self, wire: &SubagentWire) -> Result<()> {
        match wire {
            SubagentWire::Message(message) => {
                if let Some(id) = &message.recipient_session_id {
                    if let Some(message_id) = &message.message_id {
                        let mut sources = self.user_sources.lock().await;
                        let source = sources
                            .entry((id.clone(), message_id.clone()))
                            .or_insert(true);
                        if !*source {
                            return Ok(());
                        }
                    }
                    let recorder = self.recorder(id).await?;
                    let mut chunk = acp::ContentChunk::new(acp::ContentBlock::Text(
                        acp::TextContent::new(message.text.clone()),
                    ));
                    if let Some(message_id) = &message.message_id {
                        chunk = chunk.message_id(acp::MessageId::new(message_id.clone()));
                    }
                    let notification = acp::SessionNotification::new(
                        id.clone(),
                        acp::SessionUpdate::UserMessageChunk(chunk),
                    );
                    let event = SessionEvent::Acp(AcpEvent::Notification(
                        acp::AgentNotification::SessionNotification(notification),
                    ));
                    recorder
                        .record_session_event(&SessionEventEntry::new(event))
                        .await?;
                }
            }
            SubagentWire::Update(update) => {
                let outcome = match &update.state {
                    Patch::Set(SubagentState::Idle) => Some(Ok(acp::StopReason::EndTurn)),
                    Patch::Set(SubagentState::Ended(
                        nori_protocol::SubagentOutcome::Completed { stop_reason },
                    )) => Some(Ok(match stop_reason.as_deref() {
                        Some("max_tokens") => acp::StopReason::MaxTokens,
                        Some("max_turn_requests") => acp::StopReason::MaxTurnRequests,
                        Some("refusal") => acp::StopReason::Refusal,
                        Some("cancelled" | "canceled") => acp::StopReason::Cancelled,
                        _ => acp::StopReason::EndTurn,
                    })),
                    Patch::Set(SubagentState::Ended(nori_protocol::SubagentOutcome::Cancelled)) => {
                        Some(Ok(acp::StopReason::Cancelled))
                    }
                    Patch::Set(SubagentState::Ended(nori_protocol::SubagentOutcome::Failed {
                        reason,
                    })) => Some(Err(reason
                        .clone()
                        .unwrap_or_else(|| "Child failed".to_owned()))),
                    Patch::Set(SubagentState::Ended(nori_protocol::SubagentOutcome::Closed)) => {
                        Some(Err("Child session closed".to_owned()))
                    }
                    Patch::Set(SubagentState::Running) | Patch::Keep | Patch::Clear => None,
                };
                if let Some(outcome) = outcome {
                    self.finish(&update.session_id, outcome).await?;
                }
            }
        }
        Ok(())
    }

    pub(super) async fn record_user(
        &self,
        id: &acp::SessionId,
        content: &[acp::ContentBlock],
    ) -> Result<()> {
        for part in content {
            self.record_notification(&acp::SessionNotification::new(
                id.clone(),
                acp::SessionUpdate::UserMessageChunk(acp::ContentChunk::new(part.clone())),
            ))
            .await?;
        }
        Ok(())
    }

    pub(super) async fn record_descendant_event(
        &self,
        parent: &acp::SessionId,
        event: SessionEvent,
    ) -> Result<()> {
        self.recorder(parent)
            .await?
            .record_session_event(&SessionEventEntry::new(event))
            .await?;
        Ok(())
    }

    /// A native child may stop before publishing configuration. Keep its initial
    /// journal reference with the immediate owner before recording its announcement.
    pub(super) async fn record_descendant_reference(
        &self,
        parent: &acp::SessionId,
        child: &acp::SessionId,
    ) -> Result<()> {
        let child_recorder = self.recorder(child).await?;
        let parent_recorder = self.recorder(parent).await?;
        let key = format!("native-child-journal:{child}");
        if parent_recorder
            .recorded_command_payload(&key)
            .await
            .is_none()
        {
            persist_binding(
                &parent_recorder,
                &key,
                serde_json::json!({
                    "project": child_recorder.project_id(), "session": child_recorder.session_id(),
                })
                .to_string(),
            )
            .await?;
        }
        Ok(())
    }

    pub(super) async fn record_descendant_binding(
        &self,
        parent: &acp::SessionId,
        child: &acp::SessionId,
        model: String,
        options: Vec<(String, String)>,
    ) -> Result<()> {
        let child_recorder = self.recorder(child).await?;
        let parent_recorder = self.recorder(parent).await?;
        let binding = ReboundBinding {
            project: child_recorder.project_id().to_owned(),
            session: child_recorder.session_id().to_owned(),
            parent: parent.to_string(),
            agent: self.agent.clone(),
            model,
            options,
            rebound: true,
        };
        persist_binding(
            &parent_recorder,
            &format!("native-child-journal:{child}"),
            serde_json::to_string(&binding)?,
        )
        .await
    }

    pub(super) async fn record_direct_binding(
        &self,
        parent: &acp::SessionId,
        child: &acp::SessionId,
        model: String,
        options: Vec<(String, String)>,
    ) -> Result<()> {
        let child_recorder = self.recorder(child).await?;
        let parent_recorder = self
            .parent
            .read()
            .await
            .clone()
            .context("This child has no parent journal")?;
        let binding = ReboundBinding {
            project: child_recorder.project_id().to_owned(),
            session: child_recorder.session_id().to_owned(),
            parent: parent.to_string(),
            agent: self.agent.clone(),
            model,
            options,
            rebound: true,
        };
        persist_binding(
            &parent_recorder,
            &format!("native-child-journal:{child}"),
            serde_json::to_string(&binding)?,
        )
        .await
    }

    pub(super) async fn finish(
        &self,
        id: &acp::SessionId,
        result: std::result::Result<acp::StopReason, String>,
    ) -> Result<()> {
        let recorder = self.recorder(id).await?;
        let event = SessionEvent::Acp(AcpEvent::Response {
            request_id: acp::RequestId::Str(format!("native-child:{}", uuid::Uuid::new_v4())),
            response: result
                .map(|reason| acp::AgentResponse::PromptResponse(acp::PromptResponse::new(reason)))
                .map_err(|error| acp::Error::internal_error().data(error)),
        });
        recorder
            .record_session_event(&SessionEventEntry::new(event))
            .await?;
        Ok(())
    }

    /// Seed a newly created child journal from its recorded parent history once.
    pub(super) async fn bootstrap(
        &self,
        transcript: &crate::transcript::Transcript,
        ids: &[acp::SessionId],
    ) -> Result<()> {
        let parent = self
            .parent
            .read()
            .await
            .clone()
            .context("This child has no conversation journal")?;
        let mut fresh = Vec::new();
        for id in ids {
            if parent
                .recorded_command_payload(&format!("native-child-journal:{id}"))
                .await
                .is_none()
            {
                self.recorder(id).await?;
                fresh.push(id.clone());
            }
        }
        for line in &transcript.entries {
            let TranscriptEntry::SessionEvent(entry) = &line.entry else {
                continue;
            };
            if entry.historical_evidence {
                continue;
            }
            match &entry.event {
                SessionEvent::Acp(AcpEvent::ChildNotification(notification))
                    if fresh.contains(&notification.session_id) =>
                {
                    self.record_notification(notification).await?
                }
                SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                    if fresh.contains(&update.session_id) =>
                {
                    self.record_wire(&SubagentWire::Update(update.clone()))
                        .await?
                }
                SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Message(message)))
                    if message
                        .recipient_session_id
                        .as_ref()
                        .is_some_and(|id| fresh.contains(id)) =>
                {
                    self.record_wire(&SubagentWire::Message(message.clone()))
                        .await?
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(super) async fn shutdown(&self) {
        for recorder in std::mem::take(&mut *self.journals.lock().await).into_values() {
            let _ = recorder.flush().await;
        }
    }
}

async fn persist_binding(parent: &TranscriptRecorder, key: &str, payload: String) -> Result<()> {
    parent.record_command(key, payload.clone()).await?;
    anyhow::ensure!(
        parent.recorded_command_payload(key).await.as_deref() == Some(payload.as_str()),
        "Child journal binding was not persisted"
    );
    Ok(())
}
