//! Preserve an interrupted native child's history without inventing its provider config.
use super::AcpBackend;
use anyhow::Context;
use anyhow::Result;
use nori_protocol::NoriEvent;
use nori_protocol::Patch;
use nori_protocol::SessionEvent;
use nori_protocol::SubagentState;
use nori_protocol::SubagentUpdate;
use nori_protocol::SubagentWire;
use nori_protocol::acp::v1 as acp;

pub(super) const UNAVAILABLE: &str = "This child was interrupted before a complete provider configuration was recorded. Its history is archived; it cannot accept work.";

/// Only the initial journal reference is eligible. A malformed provider binding must
/// still fail restoration rather than silently losing its model or settings.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct InitialReference {
    project: String,
    session: String,
}

impl AcpBackend {
    pub(super) async fn recover_or_archive_uninitialized_native_child(
        &self,
        id: acp::SessionId,
        value: serde_json::Value,
        known_bindings: &std::collections::HashSet<String>,
    ) -> Result<Option<super::native_child_history::ReboundBinding>> {
        let reference: InitialReference = serde_json::from_value(value)
            .context("Selected descendant has an invalid durable provider binding")?;
        anyhow::ensure!(
            !reference.project.is_empty() && !reference.session.is_empty(),
            "Uninitialized child has an invalid journal reference"
        );
        let recorder = self
            .transcript_recorder
            .read()
            .await
            .clone()
            .context("The parent conversation journal is unavailable")?;
        let events = recorder.replay_child_session_events().await?;
        let original_parent = events.iter().rev().find_map(|event| match event {
            SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(update)))
                if update.session_id == id =>
            {
                Some(update.parent_session_id.clone())
            }
            _ => None,
        });
        // An initial reference can precede the lifecycle append at a crash. Keep
        // such evidence under its owning journal instead of losing the history.
        let parent = match original_parent {
            Some(parent) if known_bindings.contains(&parent.to_string()) => parent,
            Some(_) | None => self.session_id().await,
        };
        // Validate and retain the independent history before making its archived view visible.
        let loader = crate::transcript::TranscriptLoader::new(self.nori_home.clone());
        let transcript = loader
            .load_transcript(&reference.project, &reference.session)
            .await
            .context("Could not load the uninitialized child's archived history")?;
        // Configuration is journaled before its parent binding is upgraded. A shutdown
        // between those writes is recoverable from the actual advertised snapshot.
        let configuration = transcript
            .entries
            .iter()
            .rev()
            .find_map(|line| {
                let crate::transcript::TranscriptEntry::SessionEvent(entry) = &line.entry else {
                    return None;
                };
                let SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    acp::AgentNotification::SessionNotification(notification),
                )) = &entry.event
                else {
                    return None;
                };
                let acp::SessionUpdate::ConfigOptionUpdate(update) = &notification.update else {
                    return None;
                };
                Some(update.config_options.clone())
            })
            .map(super::child_control::child_model_and_options)
            .transpose()
            .map_err(anyhow::Error::msg)?;
        if let Some((model, options)) = configuration {
            let binding = super::native_child_history::ReboundBinding {
                project: reference.project,
                session: reference.session,
                parent: parent.to_string(),
                agent: self.agent_name().to_owned(),
                model,
                options,
                rebound: true,
            };
            recorder
                .record_command(
                    &format!("native-child-journal:{id}"),
                    serde_json::to_string(&binding)?,
                )
                .await?;
            return Ok(Some(binding));
        }
        self.native_children
            .register_archived(id.clone(), parent.clone(), UNAVAILABLE.into())
            .await;
        let event = SessionEvent::Nori(NoriEvent::Subagent(SubagentWire::Update(SubagentUpdate {
            parent_session_id: parent,
            session_id: id,
            agent: Patch::Keep,
            model: Patch::Keep,
            title: Patch::Keep,
            description: Patch::Set(UNAVAILABLE.into()),
            state: Patch::Set(SubagentState::Ended(nori_protocol::SubagentOutcome::Closed)),
        })));
        recorder
            .record_session_event(&crate::transcript::SessionEventEntry::new(event.clone()))
            .await?;
        self.backend_event_tx
            .send(super::BackendEvent::Public(event))
            .await?;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_never_initialized_references_are_archivable() {
        assert!(
            serde_json::from_value::<InitialReference>(serde_json::json!({
                "project": "project", "session": "session",
            }))
            .is_ok()
        );
        for extra in ["model", "agent", "parent", "rebound"] {
            let mut value = serde_json::json!({"project": "project", "session": "session"});
            value[extra] = serde_json::json!("provider metadata must be validated strictly");
            assert!(serde_json::from_value::<InitialReference>(value).is_err());
        }
    }
}

#[cfg(test)]
mod reference_tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[tokio::test]
    async fn initial_descendant_reference_belongs_to_its_immediate_owner_and_never_overwrites_config()
     {
        let home = tempfile::tempdir().unwrap();
        let root = Arc::new(
            crate::transcript::TranscriptRecorder::new(
                home.path(),
                home.path(),
                Some("mock-model".into()),
                "reference-test",
                Some("root".into()),
            )
            .await
            .unwrap(),
        );
        let history = super::super::native_child_history::NativeHistory::new(
            home.path().into(),
            home.path().into(),
            "mock-model".into(),
            "reference-test".into(),
            Arc::new(RwLock::new(Some(root))),
        );
        let parent = acp::SessionId::new("parent");
        let child = acp::SessionId::new("child");
        history
            .record_descendant_reference(&parent, &child)
            .await
            .unwrap();
        let owner = history.recorder(&parent).await.unwrap();
        let key = "native-child-journal:child";
        let reference: serde_json::Value =
            serde_json::from_str(&owner.recorded_command_payload(key).await.unwrap()).unwrap();
        assert_eq!(
            reference["session"],
            history.recorder(&child).await.unwrap().session_id()
        );
        assert_eq!(reference["rebound"], serde_json::Value::Null);
        history
            .record_descendant_binding(
                &parent,
                &child,
                "actual-model".into(),
                vec![("effort".into(), "high".into())],
            )
            .await
            .unwrap();
        let configured = owner.recorded_command_payload(key).await.unwrap();
        history
            .record_descendant_reference(&parent, &child)
            .await
            .unwrap();
        assert_eq!(owner.recorded_command_payload(key).await, Some(configured));
    }
}
