//! Restore committed native-child rebindings without reconnecting the old provider child.
use super::AcpBackend;
use super::orchestrator;
use anyhow::Context;
use anyhow::Result;
use nori_protocol::acp::v1 as acp;

pub(super) struct RestoredChild {
    pub(super) id: acp::SessionId,
    pub(super) parent: acp::SessionId,
    pub(super) events: Vec<nori_protocol::SessionEvent>,
    pub(super) retired: Vec<acp::SessionId>,
    /// What the user's view shows the child as once restored.
    pub(super) state: nori_protocol::SubagentState,
}

/// The import record is consumed once delegation recovery succeeds on this machine.
const DELEGATION_RESTORED: &str = "delegation-restored-after-move";

fn needs_move_restore(commands: &[(String, String)], hostname: &str) -> bool {
    let Some((name, payload)) = commands.last() else {
        return false;
    };
    if name != crate::portable::MOVED_IN || hostname == "unknown" {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("from")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|origin| origin != "unknown" && origin != hostname)
}

impl AcpBackend {
    pub(super) async fn needs_moved_delegation_restore(&self) -> bool {
        let Some(recorder) = self.transcript_recorder.read().await.clone() else {
            return false;
        };
        match recorder
            .journaled_commands_of(&[crate::portable::MOVED_IN, DELEGATION_RESTORED])
            .await
        {
            Ok(commands)
                if commands
                    .last()
                    .is_some_and(|(name, _)| name == crate::portable::MOVED_IN) =>
            {
                needs_move_restore(&commands, &crate::portable::hostname())
            }
            Ok(_) => false,
            Err(error) => {
                tracing::warn!(%error, "could not check whether delegation recovery is needed");
                false
            }
        }
    }

    pub(super) async fn mark_moved_delegation_restored(&self) {
        if let Some(recorder) = self.transcript_recorder.read().await.clone()
            && let Err(error) = recorder
                .record_command(DELEGATION_RESTORED, String::new())
                .await
        {
            tracing::warn!(%error, "could not record completed delegation recovery");
        }
    }
    /// How the user's view is told a restored child stands. It carries the name, title and task
    /// the orchestrator announced for it live, in the same shape; a child the orchestrator does
    /// not know keeps what the view already shows.
    pub(super) fn restored_update(&self, child: &RestoredChild) -> nori_protocol::SubagentUpdate {
        use nori_protocol::Patch;
        match self
            .orchestrator
            .announcement_for_view(&child.id, child.state.clone())
        {
            Some(update) => nori_protocol::SubagentUpdate {
                parent_session_id: child.parent.clone(),
                ..update
            },
            None => nori_protocol::SubagentUpdate {
                parent_session_id: child.parent.clone(),
                session_id: child.id.clone(),
                agent: Patch::Keep,
                model: Patch::Keep,
                name: Patch::Keep,
                title: Patch::Keep,
                description: Patch::Keep,
                state: Patch::Set(child.state.clone()),
            },
        }
    }

    pub(super) async fn managed_child_ids(&self) -> Result<Vec<acp::SessionId>> {
        let Some(recorder) = self.transcript_recorder.read().await.clone() else {
            return Ok(Vec::new());
        };
        if !recorder.has_conversation_log() {
            return Ok(Vec::new());
        }
        Ok(
            crate::transcript::fork_context::child_bindings_from_records(
                &recorder.visible_records().await?,
            )
            .into_iter()
            .filter_map(|(name, _)| {
                name.strip_prefix("managed-child-journal:")
                    .map(acp::SessionId::new)
            })
            .collect(),
        )
    }
    /// Reattach managed tasks to their original public views after root restart.
    pub(super) async fn restore_managed_children(
        &self,
        previous_root: Option<&str>,
    ) -> Result<Vec<RestoredChild>> {
        use nori_conversation::Event;
        let Some(parent_recorder) = self.transcript_recorder.read().await.clone() else {
            return Ok(Vec::new());
        };
        if !parent_recorder.has_conversation_log() {
            return Ok(Vec::new());
        }
        let mut bindings = std::collections::BTreeMap::new();
        for record in parent_recorder.visible_records().await? {
            if let Event::Command { name, payload } = record.event
                && let Some(id) = name.strip_prefix("managed-child-journal:")
            {
                if payload == "null" {
                    bindings.remove(id);
                } else {
                    bindings.insert(
                        id.to_owned(),
                        serde_json::from_str::<super::native_child_history::ReboundBinding>(
                            &payload,
                        )?,
                    );
                }
            }
        }
        let mut restored = Vec::new();
        for (public_id, binding) in bindings {
            let id = acp::SessionId::new(public_id);
            let loader = crate::transcript::TranscriptLoader::new(self.nori_home.clone());
            let transcript = loader
                .load_transcript(&binding.project, &binding.session)
                .await?;
            let source_recorder = crate::transcript::TranscriptRecorder::continue_session(
                loader.session_path(&binding.project, &binding.session),
                &transcript.meta,
            )
            .await?;
            let records = source_recorder.visible_records().await?;
            let start = records
                .first()
                .context("Managed child journal has no visible history")?
                .id;
            let end = records
                .last()
                .context("Managed child journal has no visible history")?
                .id;
            let source = (binding.session.clone(), start, end);
            let context = crate::transcript::fork_context::context_from_records(records.clone())?;
            let entries =
                crate::transcript::fork_context::replay_entries_with_children_from_records(
                    &records, &source.0,
                );
            let nested_bindings =
                crate::transcript::fork_context::child_bindings_from_records(&records);
            let external_managed = self.managed_child_ids().await?;
            let parent = if previous_root == Some(binding.parent.as_str()) {
                self.session_id().await
            } else {
                acp::SessionId::new(binding.parent.clone())
            };
            let staged = self
                .orchestrator
                .prepare_native_rebind(orchestrator::NativeRebindSpec {
                    public_id: id.clone(),
                    parent_id: parent.clone(),
                    agent: binding.agent.clone(),
                    model: Some(binding.model.clone()),
                    options: binding.options.clone(),
                    context,
                    source,
                    entries,
                    nested_bindings,
                    external_managed,
                })
                .await
                .map_err(anyhow::Error::msg)?;
            let staged_recorder = staged
                .handle()
                .conversation_recorder()
                .await?
                .context("Restored managed child has no journal")?;
            let replacement = super::native_child_history::ReboundBinding {
                project: staged_recorder.project_id().to_owned(),
                session: staged_recorder.session_id().to_owned(),
                parent: parent.to_string(),
                ..binding
            };
            self.orchestrator
                .commit_native_rebind(staged)
                .await
                .map_err(anyhow::Error::msg)?;
            if let Err(error) = parent_recorder
                .record_command(
                    &format!("managed-child-journal:{id}"),
                    serde_json::to_string(&replacement)?,
                )
                .await
            {
                let _ = self.orchestrator.rollback_native_rebind(&id).await;
                return Err(error.into());
            }
            if let Err(error) = self.orchestrator.activate_native_rebind(&id).await {
                let _ = self.orchestrator.rollback_native_rebind(&id).await;
                return Err(anyhow::Error::msg(error));
            }
            let events = staged_recorder.replay_child_session_events().await?;
            restored.push(RestoredChild {
                id: id.clone(),
                parent,
                events,
                retired: vec![id],
                state: nori_protocol::SubagentState::Idle { interrupted: true },
            });
        }
        // The rest of the tree: children, mail and task history of the previous run, dormant.
        if previous_root.is_some() {
            restored.extend(self.restore_actor_directory(&parent_recorder).await);
        }
        Ok(restored)
    }
    /// Restore descendant bindings copied into an unpublished staged child journal.
    pub(crate) async fn restore_seeded_native_children(&self) -> Result<()> {
        use nori_protocol::NoriEvent;
        use nori_protocol::Patch;
        use nori_protocol::SessionEvent;
        use nori_protocol::SubagentState;
        use nori_protocol::SubagentUpdate;
        use nori_protocol::SubagentWire;
        let recorder = self
            .transcript_recorder
            .read()
            .await
            .clone()
            .context("Staged child has no journal")?;
        let records = recorder.visible_records().await?;
        let bindings = crate::transcript::fork_context::child_bindings_from_records(&records);
        let all_ids: std::collections::HashSet<_> = bindings
            .iter()
            .filter_map(|(name, _)| {
                name.strip_prefix("native-child-journal:")
                    .or_else(|| name.strip_prefix("managed-child-journal:"))
            })
            .map(str::to_owned)
            .collect();
        let mut children = Vec::new();
        let root = self.session_id().await;
        for (name, payload) in bindings {
            if name.starts_with("managed-child-journal:") {
                let mut binding: super::native_child_history::ReboundBinding =
                    serde_json::from_str(&payload)?;
                if !all_ids.contains(&binding.parent) {
                    binding.parent = root.to_string();
                    recorder
                        .record_command(&name, serde_json::to_string(&binding)?)
                        .await?;
                }
                continue;
            }
            let id = acp::SessionId::new(name.trim_start_matches("native-child-journal:"));
            let mut value: serde_json::Value = serde_json::from_str(&payload)?;
            if value.get("rebound").is_none() {
                let Some(recovered) = self
                    .recover_or_archive_uninitialized_native_child(id.clone(), value, &all_ids)
                    .await?
                else {
                    continue;
                };
                value = serde_json::to_value(recovered)?;
            }
            anyhow::ensure!(
                value.get("rebound").and_then(serde_json::Value::as_bool) == Some(true),
                "Selected descendant has an invalid independent durable binding"
            );
            let parent = value
                .get("parent")
                .and_then(serde_json::Value::as_str)
                .context("Selected descendant has no parent identity")?
                .to_owned();
            children.push((id, parent));
        }
        let mut pending = children;
        while !pending.is_empty() {
            let index = pending
                .iter()
                .position(|(_, parent)| !pending.iter().any(|(id, _)| id.to_string() == *parent))
                .context("Selected descendant hierarchy contains a cycle")?;
            let (id, parent) = pending.remove(index);
            let parent_id = if all_ids.contains(&parent) {
                acp::SessionId::new(parent.clone())
            } else {
                root.clone()
            };
            let update = SubagentUpdate {
                parent_session_id: parent_id,
                session_id: id.clone(),
                agent: Patch::Keep,
                model: Patch::Keep,
                title: Patch::Keep,
                description: Patch::Keep,
                state: Patch::Keep,
            };
            self.native_children
                .observe_replayed(
                    &self.connection,
                    &self.backend_event_tx,
                    &root,
                    &SubagentWire::Update(update),
                )
                .await;
            // Rebase only the selected binding's original owning session to this new engine.
            if !all_ids.contains(&parent) {
                let mut binding = self
                    .native_children
                    .rebound_binding(&id)
                    .await?
                    .context("Selected descendant binding disappeared")?;
                binding.parent = root.to_string();
                recorder
                    .record_command(
                        &format!("native-child-journal:{id}"),
                        serde_json::to_string(&binding)?,
                    )
                    .await?;
            }
        }
        let mut restored = self.restore_native_children(None).await?;
        restored.extend(self.restore_managed_children(None).await?);
        for child in restored {
            let id = child.id.clone();
            let update = self.restored_update(&child);
            self.backend_event_tx
                .send(super::BackendEvent::Public(SessionEvent::Nori(
                    NoriEvent::Subagent(SubagentWire::Update(update)),
                )))
                .await?;
            for event in child.events {
                if let SessionEvent::Acp(nori_protocol::AcpEvent::Notification(
                    acp::AgentNotification::SessionNotification(mut notification),
                )) = event
                {
                    notification.session_id = id.clone();
                    self.backend_event_tx
                        .send(super::BackendEvent::Public(SessionEvent::Acp(
                            nori_protocol::AcpEvent::ChildNotification(notification),
                        )))
                        .await?;
                }
            }
        }
        Ok(())
    }
    pub(super) async fn restore_native_children(
        &self,
        previous_root: Option<&str>,
    ) -> Result<Vec<RestoredChild>> {
        let mut restored = Vec::new();
        let root = self.session_id().await;
        let mut pending = Vec::new();
        for id in self.native_children.known_ids().await {
            if let Some(binding) = self.native_children.rebound_binding(&id).await? {
                pending.push((id, binding));
            }
        }
        while !pending.is_empty() {
            let index = pending
                .iter()
                .position(|(_, binding)| {
                    !pending
                        .iter()
                        .any(|(id, _)| id.to_string() == binding.parent)
                })
                .context("Restored child hierarchy contains a cycle")?;
            let (id, binding) = pending.remove(index);
            // Never let an old provider child receive requests while its replacement is restored.
            let retired = self.native_children.subtree(&id).await;
            for old in &retired {
                self.native_children.suppress(old, true).await;
            }
            pending.retain(|(candidate, _)| candidate == &id || !retired.contains(candidate));
            let recorder = self.native_children.history(&id).await?;
            let records = recorder.visible_records().await?;
            let start = records
                .first()
                .context("Child journal has no visible history")?
                .id;
            let end = records
                .last()
                .context("Child journal has no visible history")?
                .id;
            let source = (recorder.session_id().to_owned(), start, end);
            let context = crate::transcript::fork_context::context_from_records(records.clone())?;
            let entries =
                crate::transcript::fork_context::replay_entries_with_children_from_records(
                    &records, &source.0,
                );
            let nested_bindings =
                crate::transcript::fork_context::child_bindings_from_records(&records);
            let external_managed = self.managed_child_ids().await?;
            let parent = if previous_root == Some(binding.parent.as_str()) {
                root.clone()
            } else {
                acp::SessionId::new(binding.parent.clone())
            };
            let staged = self
                .orchestrator
                .prepare_native_rebind(orchestrator::NativeRebindSpec {
                    public_id: id.clone(),
                    parent_id: parent.clone(),
                    agent: binding.agent.clone(),
                    model: Some(binding.model.clone()),
                    options: binding.options.clone(),
                    context,
                    source,
                    entries,
                    nested_bindings,
                    external_managed,
                })
                .await
                .map_err(anyhow::Error::msg)?;
            let staged_recorder = staged
                .handle()
                .conversation_recorder()
                .await?
                .context("Restored child has no conversation journal")?;
            let replacement = super::native_child_history::ReboundBinding {
                project: staged_recorder.project_id().to_owned(),
                session: staged_recorder.session_id().to_owned(),
                parent: parent.to_string(),
                ..binding
            };
            self.orchestrator
                .commit_native_rebind(staged)
                .await
                .map_err(anyhow::Error::msg)?;
            if let Err(error) = self
                .native_children
                .bind_rebound(&id, staged_recorder.clone(), replacement)
                .await
            {
                let _ = self.orchestrator.rollback_native_rebind(&id).await;
                return Err(error);
            }
            if let Err(error) = self.orchestrator.activate_native_rebind(&id).await {
                let _ = self.orchestrator.rollback_native_rebind(&id).await;
                return Err(anyhow::Error::msg(error));
            }
            for old in &retired {
                self.native_children.retire(old).await;
            }
            let events = staged_recorder.replay_child_session_events().await?;
            restored.push(RestoredChild {
                id,
                parent,
                events,
                retired,
                state: nori_protocol::SubagentState::Idle { interrupted: true },
            });
        }
        Ok(restored)
    }
}

#[cfg(test)]
mod move_restore_tests {
    use super::*;

    #[test]
    fn only_a_new_unrestored_import_needs_delegation_recovery() {
        let moved = (
            crate::portable::MOVED_IN.to_owned(),
            r#"{"from":"old-host"}"#.to_owned(),
        );
        let restored = (DELEGATION_RESTORED.to_owned(), String::new());
        assert!(!needs_move_restore(&[], "new-host"));
        assert!(needs_move_restore(std::slice::from_ref(&moved), "new-host"));
        assert!(!needs_move_restore(
            std::slice::from_ref(&moved),
            "old-host"
        ));
        assert!(!needs_move_restore(
            &[moved.clone(), restored.clone()],
            "new-host"
        ));
        assert!(needs_move_restore(
            &[moved.clone(), restored, moved],
            "new-host"
        ));
    }
}
