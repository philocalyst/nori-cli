//! Persist the identity and exact configuration of an independently managed history fork.
use super::Inner;
use super::child::Child;
use nori_protocol::acp::v1 as acp;

impl Inner {
    pub(super) async fn persist_managed_fork(
        &self,
        child: &Child,
        view: &acp::SessionId,
        parent: Option<acp::SessionId>,
        agent: String,
    ) -> Result<(), String> {
        let backend = self
            .program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| "The parent session is unavailable".to_owned())?;
        let root = backend
            .transcript_recorder
            .read()
            .await
            .clone()
            .ok_or_else(|| "The parent conversation journal is unavailable".to_owned())?;
        let recorder = child
            .harness
            .conversation_recorder()
            .await
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "The child conversation journal is unavailable".to_owned())?;
        let snapshot = child.harness.get_session_config().await;
        let (model, options) = match snapshot {
            Some(snapshot) => {
                match super::super::child_control::child_model_and_options(snapshot.clone()) {
                    Ok(parsed) => parsed,
                    Err(_) => {
                        // Non-Claude adapters (e.g. Gemini or OpenCode without a model selector)
                        // may omit a model config option; preserve select options and fall back
                        // to the configured default model so checkpoint-seeded fork succeeds.
                        let options = snapshot
                            .into_iter()
                            .filter_map(|option| match option.kind {
                                acp::SessionConfigKind::Select(select) => {
                                    Some((option.id.to_string(), select.current_value.to_string()))
                                }
                                _ => None,
                            })
                            .collect();
                        let fallback_model = self
                            .seed
                            .config
                            .default_models
                            .get(&agent)
                            .cloned()
                            .unwrap_or_else(|| "default".to_owned());
                        (fallback_model, options)
                    }
                }
            }
            None => {
                let fallback_model = self
                    .seed
                    .config
                    .default_models
                    .get(&agent)
                    .cloned()
                    .unwrap_or_else(|| "default".to_owned());
                (fallback_model, Vec::new())
            }
        };
        let parent = match parent {
            Some(parent) => parent,
            None => backend.session_id().await,
        };
        let binding = super::super::native_child_history::ReboundBinding {
            project: recorder.project_id().to_owned(),
            session: recorder.session_id().to_owned(),
            parent: parent.to_string(),
            agent,
            model,
            options,
            rebound: true,
        };
        let payload = serde_json::to_string(&binding).map_err(|error| error.to_string())?;
        let name = format!("managed-child-journal:{view}");
        root.record_command(&name, payload.clone())
            .await
            .map_err(|error| error.to_string())?;
        if root.recorded_command_payload(&name).await.as_deref() != Some(payload.as_str()) {
            return Err("The managed child journal binding could not be verified".to_owned());
        }
        Ok(())
    }
    pub(super) async fn remove_managed_fork_binding(
        &self,
        view: &acp::SessionId,
    ) -> Result<(), String> {
        let backend = self
            .program_backend
            .get()
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| "The parent session is unavailable".to_owned())?;
        let root = backend
            .transcript_recorder
            .read()
            .await
            .clone()
            .ok_or_else(|| "The parent conversation journal is unavailable".to_owned())?;
        let name = format!("managed-child-journal:{view}");
        root.record_command(&name, "null".to_owned())
            .await
            .map_err(|error| error.to_string())?;
        if root.recorded_command_payload(&name).await.as_deref() != Some("null") {
            return Err("The managed child journal removal could not be verified".to_owned());
        }
        Ok(())
    }
}
