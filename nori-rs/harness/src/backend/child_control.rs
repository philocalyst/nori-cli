//! The user's and programs' controls over a child session, resolved through the actor directory.
//!
//! A control names its child by a session id the user's view shows, or by any actor address
//! (`alias`, `actor://alias`). [`AcpBackend::control_target`] is the one place that decides what
//! that is: a child Nori manages, a provider's own child, or a session further down that another
//! connection relays. Every control then acts on that one answer.

mod rewind;

use nori_protocol::acp::v1 as acp;

use super::orchestrator::DescendantRoute;

/// Who handles a control, once its target is resolved.
enum Control {
    /// A child Nori manages.
    Managed,
    /// A child the provider runs inside its own session.
    Native,
    /// A session more than one hop down, behind another connection.
    Hop(DescendantRoute),
}

impl super::AcpBackend {
    async fn classify(&self, id: &acp::SessionId) -> Option<Control> {
        if self.orchestrator.owns_view(id).await {
            return Some(Control::Managed);
        }
        if self.native_children.owns(id).await {
            return Some(Control::Native);
        }
        self.orchestrator
            .descendant_next_hop(id)
            .await
            .map(Control::Hop)
    }

    /// Resolve what `session_id` names. Addresses that are not a session id the view shows (an
    /// alias, an `actor://` form) resolve through the directory first.
    async fn control_target(
        &self,
        session_id: &acp::SessionId,
    ) -> anyhow::Result<(acp::SessionId, Control)> {
        if let Some(control) = self.classify(session_id).await {
            return Ok((session_id.clone(), control));
        }
        if let Some(resolved) = self
            .orchestrator
            .resolve_actor_view(&session_id.to_string())
            && let Some(control) = self.classify(&resolved).await
        {
            return Ok((resolved, control));
        }
        anyhow::bail!(
            "`{session_id}` is not a child session or an actor of this conversation; a child is \
             addressed by its session id, its alias, or `actor://alias`"
        )
    }

    /// The gates every mutating control holds: the child's own, and the rewind admission.
    async fn admit_child_control(
        &self,
        session_id: &acp::SessionId,
        rewound: &'static str,
    ) -> anyhow::Result<(
        tokio::sync::OwnedMutexGuard<()>,
        tokio::sync::RwLockReadGuard<'_, ()>,
    )> {
        let child_gate = self.native_children.control_gate(session_id).await;
        let generation = self.orchestrator.admission_generation();
        let admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            rewound
        );
        Ok((child_gate, admission))
    }

    pub(crate) async fn controlled_subagent_handle(
        &self,
        session_id: &acp::SessionId,
    ) -> Option<crate::runtime::HarnessHandle> {
        let _child_gate = self.native_children.control_gate(session_id).await;
        if self.native_children.owns(session_id).await {
            return None;
        }
        self.orchestrator
            .controlled_subagent_handle(session_id)
            .await
    }

    /// The actor an address names: an alias, `actor://alias`, a session id, or a `task-N`.
    pub(crate) fn actor_resolve(
        &self,
        address: &str,
    ) -> anyhow::Result<super::orchestrator::ActorView> {
        self.orchestrator
            .resolve_actor(address)
            .map_err(anyhow::Error::msg)
    }

    /// Message an actor; returns the task that tracks the turn.
    pub(crate) async fn actor_send(
        &self,
        address: &str,
        body: String,
        steer: bool,
    ) -> anyhow::Result<super::orchestrator::TaskSnapshot> {
        let generation = self.orchestrator.admission_generation();
        let _admission = self.program_effects.read().await;
        anyhow::ensure!(
            generation == self.orchestrator.admission_generation(),
            "The conversation was rewound before the message was sent"
        );
        self.orchestrator
            .send_task(address, body, steer)
            .await
            .map_err(anyhow::Error::msg)
    }

    /// Wait for a task to end and return its result.
    pub(crate) async fn actor_await(
        &self,
        task: &str,
        timeout: std::time::Duration,
    ) -> anyhow::Result<super::orchestrator::TaskSnapshot> {
        self.orchestrator
            .await_task(task, timeout)
            .await
            .map_err(anyhow::Error::msg)
    }

    /// Stop a task.
    pub(crate) async fn actor_cancel(&self, task: &str) -> anyhow::Result<()> {
        self.orchestrator
            .cancel_task_by_id(task)
            .await
            .map(drop)
            .map_err(anyhow::Error::msg)
    }

    /// Queue a message from the user (or a conversation program) for a child.
    pub(crate) async fn send_subagent_content(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
    ) -> anyhow::Result<()> {
        let (id, target) = self.control_target(&session_id).await?;
        let _gates = self
            .admit_child_control(
                &id,
                "The conversation was rewound before the child message started",
            )
            .await?;
        match target {
            Control::Managed => self
                .orchestrator
                .send_user_content_message_locked(id, content)
                .await
                .map_err(anyhow::Error::msg),
            Control::Native => self.native_children.enqueue(&id, content).await,
            Control::Hop(next) => {
                next.handle
                    .send_subagent_content(next.session_id, content)
                    .await
            }
        }
    }

    pub(crate) async fn steer_subagent_content(
        &self,
        session_id: acp::SessionId,
        content: Vec<acp::ContentBlock>,
    ) -> anyhow::Result<()> {
        let (id, target) = self.control_target(&session_id).await?;
        let _gates = self
            .admit_child_control(
                &id,
                "The conversation was rewound before the child message started",
            )
            .await?;
        match target {
            Control::Managed => self
                .orchestrator
                .steer_user_content_message_locked(id, content)
                .await
                .map_err(anyhow::Error::msg),
            Control::Native => self.native_children.steer(&id, content).await,
            Control::Hop(next) => {
                next.handle
                    .steer_subagent_content(next.session_id, content)
                    .await
            }
        }
    }

    pub(crate) async fn promote_subagent_queue(
        &self,
        session_id: acp::SessionId,
    ) -> anyhow::Result<()> {
        let (id, target) = self.control_target(&session_id).await?;
        let _gates = self
            .admit_child_control(
                &id,
                "The conversation was rewound before the child operation started",
            )
            .await?;
        match target {
            Control::Managed => self
                .orchestrator
                .promote_user_queue(&id)
                .await
                .map_err(anyhow::Error::msg),
            Control::Native => self.native_children.promote(&id).await,
            Control::Hop(next) => next.handle.promote_subagent_queue(next.session_id).await,
        }
    }

    pub(crate) async fn cancel_subagent(&self, session_id: acp::SessionId) -> anyhow::Result<()> {
        let (id, target) = self.control_target(&session_id).await?;
        let _gates = self
            .admit_child_control(
                &id,
                "The conversation was rewound before the child operation started",
            )
            .await?;
        match target {
            Control::Managed => self
                .orchestrator
                .cancel_user_turn(&id)
                .await
                .map_err(anyhow::Error::msg),
            Control::Native => self.native_children.cancel(&id).await,
            Control::Hop(next) => next.handle.cancel_subagent(next.session_id).await,
        }
    }

    /// The handle of a managed child's own control plane.
    async fn managed_child(
        &self,
        session_id: &acp::SessionId,
    ) -> anyhow::Result<crate::runtime::HarnessHandle> {
        self.orchestrator
            .controlled_subagent_handle(session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Child session is unavailable"))
    }

    pub(crate) async fn child_config_options(
        &self,
        session_id: acp::SessionId,
    ) -> anyhow::Result<Vec<acp::SessionConfigOption>> {
        let (id, target) = self.control_target(&session_id).await?;
        let _child_gate = self.native_children.control_gate(&id).await;
        match target {
            Control::Managed => self
                .managed_child(&id)
                .await?
                .get_session_config()
                .await
                .ok_or_else(|| anyhow::anyhow!("Child session config is unavailable")),
            Control::Native => self.native_children.config_options(&id).await,
            Control::Hop(next) => next.handle.child_config_options(next.session_id).await,
        }
    }

    pub(crate) async fn set_child_config_option(
        &self,
        session_id: acp::SessionId,
        config_id: String,
        value: String,
    ) -> anyhow::Result<Vec<acp::SessionConfigOption>> {
        let (id, target) = self.control_target(&session_id).await?;
        let _gates = self
            .admit_child_control(
                &id,
                "The conversation was rewound before the child configuration changed",
            )
            .await?;
        match target {
            Control::Managed => {
                let child = self.managed_child(&id).await?;
                let updated = child.set_session_config_option(config_id, value).await?;
                self.persist_child_config(&id, &child, &updated).await?;
                Ok(updated)
            }
            Control::Native => {
                self.native_children
                    .set_config_option(&id, config_id.into(), value.into())
                    .await
            }
            Control::Hop(next) => {
                next.handle
                    .set_child_config_option(next.session_id, config_id, value)
                    .await
            }
        }
    }

    /// Save a managed child's changed model and settings in the bindings that restore it: the
    /// rebound native binding and the parent journal's managed-child binding.
    async fn persist_child_config(
        &self,
        id: &acp::SessionId,
        child: &crate::runtime::HarnessHandle,
        updated: &[acp::SessionConfigOption],
    ) -> anyhow::Result<()> {
        if self.native_children.is_suppressed(id).await
            && let Some(mut binding) = self.native_children.rebound_binding(id).await?
        {
            let (model, options) = child_model_and_options(updated.to_vec())?;
            let recorder = child
                .conversation_recorder()
                .await?
                .ok_or_else(|| anyhow::anyhow!("This child has no conversation journal"))?;
            binding.project = recorder.project_id().to_owned();
            binding.session = recorder.session_id().to_owned();
            binding.model = model;
            binding.options = options;
            self.native_children
                .bind_rebound(id, recorder, binding)
                .await?;
        }
        if let Some(parent) = self.transcript_recorder.read().await.clone() {
            let key = format!("managed-child-journal:{id}");
            if let Some(payload) = parent.recorded_command_payload(&key).await
                && payload != "null"
            {
                let mut binding: super::native_child_history::ReboundBinding =
                    serde_json::from_str(&payload)?;
                let (model, options) = child_model_and_options(updated.to_vec())?;
                let recorder = child
                    .conversation_recorder()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("This child has no conversation journal"))?;
                binding.project = recorder.project_id().to_owned();
                binding.session = recorder.session_id().to_owned();
                binding.model = model;
                binding.options = options;
                let payload = serde_json::to_string(&binding)?;
                parent.record_command(&key, payload.clone()).await?;
                anyhow::ensure!(
                    parent.recorded_command_payload(&key).await.as_deref()
                        == Some(payload.as_str()),
                    "Managed child configuration was not persisted"
                );
            }
        }
        Ok(())
    }

    pub(crate) async fn child_history(
        &self,
        session_id: acp::SessionId,
    ) -> anyhow::Result<Vec<nori_protocol::SessionEvent>> {
        let (id, target) = self.control_target(&session_id).await?;
        let _child_gate = self.native_children.control_gate(&id).await;
        match target {
            Control::Managed => {
                let recorder = self
                    .managed_child(&id)
                    .await?
                    .conversation_recorder()
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("This child has no conversation journal"))?;
                Ok(recorder.replay_child_session_events().await?)
            }
            Control::Native => Ok(self
                .native_children
                .history(&id)
                .await?
                .replay_child_session_events()
                .await?),
            Control::Hop(next) => next.handle.child_history(next.session_id).await,
        }
    }

    pub(crate) async fn fork_child_history(
        &self,
        session_id: acp::SessionId,
        context: String,
        source: (String, i64, i64),
    ) -> anyhow::Result<()> {
        let (id, target) = self.control_target(&session_id).await?;
        let _child_gate = self.native_children.control_gate(&id).await;
        match target {
            Control::Managed => {
                self.managed_child(&id)
                    .await?
                    .fork_history(context, Some(source))
                    .await
            }
            Control::Hop(next) => {
                next.handle
                    .fork_child_history(next.session_id, context, source)
                    .await
            }
            Control::Native => {
                let generation = self.orchestrator.admission_generation();
                let _admission = self.program_effects.read().await;
                anyhow::ensure!(
                    generation == self.orchestrator.admission_generation(),
                    "The conversation was rewound before the child fork started"
                );
                let recorder = self.native_children.history(&id).await?;
                let records = recorder
                    .projected_fork_records(&source.0, source.1, source.2)
                    .await?;
                let entries = crate::transcript::fork_context::replay_entries_from_records(
                    &records, &source.0,
                );
                let context = crate::transcript::fork_context::context_from_records(
                    records,
                    crate::context_budget::ContextBudget::for_agent(self.agent_name()),
                )?;
                let (model, options) = self.native_child_model_and_options(&id).await?;
                self.orchestrator
                    .fork_native_history(
                        id,
                        self.agent_name().to_owned(),
                        Some(model),
                        options,
                        context,
                        source,
                        entries,
                    )
                    .await
                    .map_err(anyhow::Error::msg)
            }
        }
    }

    async fn native_child_model_and_options(
        &self,
        session_id: &acp::SessionId,
    ) -> anyhow::Result<(String, Vec<(String, String)>)> {
        child_model_and_options(self.native_children.config_options(session_id).await?)
    }
}

pub(super) fn child_model_and_options(
    snapshot: Vec<acp::SessionConfigOption>,
) -> anyhow::Result<(String, Vec<(String, String)>)> {
    let mut model = None;
    let mut options = Vec::new();
    for option in snapshot {
        if let acp::SessionConfigKind::Select(select) = option.kind {
            let value = select.current_value.to_string();
            if option.category == Some(acp::SessionConfigOptionCategory::Model)
                || option.id.to_string() == "model"
            {
                model = Some(value);
            } else {
                options.push((option.id.to_string(), value));
            }
        } else {
            return Err(anyhow::anyhow!(
                "Cannot preserve an unsupported child configuration option"
            ));
        }
    }
    Ok((
        model.ok_or_else(|| {
            anyhow::anyhow!("The child has not advertised its model configuration")
        })?,
        options,
    ))
}
