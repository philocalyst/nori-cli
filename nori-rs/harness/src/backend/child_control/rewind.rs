//! Rewinding a child to an earlier point in its history.

use nori_protocol::acp::v1 as acp;

use super::Control;

impl crate::backend::AcpBackend {
    pub(crate) async fn rewind_child_history(
        &self,
        session_id: acp::SessionId,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        let (id, target) = self.control_target(&session_id).await?;
        let _child_gate = self.native_children.control_gate(&id).await;
        match target {
            Control::Managed => {
                self.managed_child(&id)
                    .await?
                    .rewind_and_rebind(source.0, Some(source.2))
                    .await
            }
            Control::Hop(next) => {
                next.handle
                    .rewind_child_history(next.session_id, source)
                    .await
            }
            Control::Native => self.rewind_native_child(id, source).await,
        }
    }

    /// A provider's child cannot rewind in place: its selected history is restored into an
    /// independent managed session that takes over the view, atomically with its subtree.
    async fn rewind_native_child(
        &self,
        session_id: acp::SessionId,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        let generation = self.orchestrator.admission_generation();
        let _admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the child operation started"
        );
        self.rebind_native_child(session_id, source).await
    }

    /// Restart an ended provider child as a managed actor with the same identity (session id,
    /// alias, parent), seeded with everything recorded of its history. It takes the child's
    /// control gate itself, so callers must not hold it.
    pub(crate) async fn revive_native_child(
        &self,
        session_id: acp::SessionId,
    ) -> anyhow::Result<()> {
        let _child_gate = self.native_children.control_gate(&session_id).await;
        let recorder = self.native_children.history(&session_id).await?;
        let records = recorder.visible_records().await?;
        let (Some(first), Some(last)) = (records.first(), records.last()) else {
            anyhow::bail!("no history was recorded for it");
        };
        let source = (recorder.session_id().to_owned(), first.id, last.id);
        self.rebind_native_child(session_id, source).await
    }

    /// Replace a provider child by an independent managed session seeded with `source`, taking
    /// over its view atomically with its subtree. The caller holds the child's control gate (and
    /// the rewind admission, when it is a user rewind).
    async fn rebind_native_child(
        &self,
        session_id: acp::SessionId,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        self.native_children.reserve(&session_id).await?;
        let subtree = self.native_children.subtree(&session_id).await;
        let mut descendant_ids = subtree
            .iter()
            .filter(|id| *id != &session_id)
            .cloned()
            .collect::<Vec<_>>();
        descendant_ids.sort_by_key(ToString::to_string);
        let mut descendant_gates = Vec::new();
        let mut reserved = Vec::new();
        for id in &descendant_ids {
            descendant_gates.push(self.native_children.control_gate(id).await);
            if let Err(error) = self.native_children.reserve(id).await {
                for reserved_id in &reserved {
                    self.native_children.release(reserved_id).await;
                }
                self.native_children.release(&session_id).await;
                return Err(error);
            }
            reserved.push(id.clone());
        }
        let result = async {
            let recorder = self.native_children.history(&session_id).await?;
            let records = recorder
                .projected_fork_records(&source.0, source.1, source.2)
                .await?;
            let context = crate::transcript::fork_context::context_from_records(
                records.clone(),
                crate::context_budget::ContextBudget::for_agent(self.agent_name()),
            )?;
            let entries =
                crate::transcript::fork_context::replay_entries_with_children_from_records(
                    &records, &source.0,
                );
            let nested_bindings =
                crate::transcript::fork_context::child_bindings_from_records(&records);
            let external_managed = self.managed_child_ids().await?;
            let (model, options) = self.native_child_model_and_options(&session_id).await?;
            let parent_id = self.native_children.parent_id(&session_id).await?;
            let agent = self.agent_name().to_owned();
            let staged = self
                .orchestrator
                .prepare_native_rebind(crate::backend::orchestrator::NativeRebindSpec {
                    public_id: session_id.clone(),
                    parent_id: parent_id.clone(),
                    agent: agent.clone(),
                    model: Some(model.clone()),
                    options: options.clone(),
                    context,
                    source,
                    entries,
                    nested_bindings,
                    external_managed,
                })
                .await
                .map_err(anyhow::Error::msg)?;
            let staged_recorder = match staged.handle().conversation_recorder().await {
                Ok(Some(recorder)) => recorder,
                Ok(None) => {
                    let _ = staged.handle().shutdown().await;
                    return Err(anyhow::anyhow!(
                        "The staged child has no conversation journal"
                    ));
                }
                Err(error) => {
                    let _ = staged.handle().shutdown().await;
                    return Err(error);
                }
            };
            let _event_gate = self.native_children.adoption_gate().await;
            if let Err(error) = self.native_children.check_reservation(&session_id).await {
                let _ = staged.handle().shutdown().await;
                return Err(error);
            }
            anyhow::ensure!(
                self.native_children.subtree(&session_id).await.len() == subtree.len(),
                "The child hierarchy changed during rewind"
            );
            for id in &descendant_ids {
                self.native_children.check_reservation(id).await?;
            }
            for id in &subtree {
                self.native_children.suppress(id, true).await;
            }
            if let Err(error) = self.orchestrator.commit_native_rebind(staged).await {
                for id in &subtree {
                    self.native_children.suppress(id, false).await;
                }
                return Err(anyhow::Error::msg(error));
            }
            let binding = crate::backend::native_child_history::ReboundBinding {
                project: staged_recorder.project_id().to_owned(),
                session: staged_recorder.session_id().to_owned(),
                parent: parent_id.to_string(),
                agent,
                model,
                options,
                rebound: true,
            };
            if let Err(error) = self
                .native_children
                .bind_rebound(&session_id, staged_recorder, binding)
                .await
            {
                let _ = self.orchestrator.rollback_native_rebind(&session_id).await;
                return Err(error);
            }
            if let Err(error) = self.orchestrator.activate_native_rebind(&session_id).await {
                let rolled_back = self
                    .orchestrator
                    .rollback_native_rebind(&session_id)
                    .await
                    .is_ok();
                if rolled_back
                    && self
                        .native_children
                        .bind_native(&session_id, recorder)
                        .await
                        .is_ok()
                {
                    for id in &subtree {
                        self.native_children.suppress(id, false).await;
                    }
                }
                return Err(anyhow::Error::msg(error));
            }
            for id in &subtree {
                self.native_children.retire(id).await;
            }
            Ok(())
        }
        .await;
        for id in &reserved {
            self.native_children.release(id).await;
        }
        if result.is_err() {
            self.native_children.release(&session_id).await;
        }
        result
    }
}
