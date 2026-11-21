//! The agent's requests for the user: permissions and questions.
//!
//! Every request is registered with the ledger, which tracks it in the turn that owns it
//! whatever kind of turn that is. Approval policy or the user answers it. Nothing here answers
//! a request as cancelled except what the ledger asks for: the user cancelled the owning turn, or
//! the connection died.

use super::super::*;

use super::super::gates::Gated;
use super::super::session_reducer::InboundEvent;

impl AcpBackend {
    pub(in crate::backend) async fn handle_permission_request(
        &self,
        pending_request: Box<PendingApprovalRequest>,
        current_policy: AskForApproval,
    ) {
        if let acp::AgentRequest::ExtMethodRequest(request) = &pending_request.request.request
            && request.method.as_ref() == "_nori/request_user_input"
        {
            let params = serde_json::from_str::<serde_json::Value>(request.params.get()).ok();
            let key = params
                .as_ref()
                .and_then(super::super::user_question::question_key);
            if let Some(key) = key
                && self.withdrawn_questions.lock().await.remove(&key)
            {
                let _ = pending_request
                    .request
                    .response_tx
                    .send(Err(acp::Error::internal_error()));
                return;
            }
            let request_id = pending_request.request.request_id.clone();
            let event = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Request {
                request_id: request_id.clone(),
                request: pending_request.request.request.clone(),
            });
            self.pending_approvals.lock().await.push(*pending_request);
            if self
                .open_request(request_id.to_string(), None, "question".to_string())
                .await
            {
                let _ = self
                    .backend_event_tx
                    .send(BackendEvent::Public(event))
                    .await;
            }
            return;
        }
        let acp::AgentRequest::RequestPermissionRequest(permission) =
            &pending_request.request.request
        else {
            let _ = pending_request
                .request
                .response_tx
                .send(Err(acp::Error::method_not_found()));
            return;
        };
        let request_id = pending_request.request.request_id.to_string();
        let call_id = permission.tool_call.tool_call_id.to_string();
        let title = permission
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "tool request".to_string());
        let public_event = nori_protocol::SessionEvent::Acp(nori_protocol::AcpEvent::Request {
            request_id: pending_request.request.request_id.clone(),
            request: pending_request.request.request.clone(),
        });
        // Nori never asks permission: a tool the agent wants to run runs. A gate is the person's
        // own standing rule and still speaks first; a request the agent marks as a question for
        // the person (`_meta.nori.question`, such as Claude's plan review) is the only kind shown.
        let question = permission
            .meta
            .as_ref()
            .and_then(|meta| meta.get("nori"))
            .and_then(|nori| nori.get("question"))
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let gated = self.gate_permission(permission, current_policy, None).await;
        let mut answer_meta = None;
        let auto_outcome = match gated {
            Gated::Answer { outcome, meta } => {
                answer_meta = meta;
                Some(outcome)
            }
            Gated::Ask => None,
            Gated::Unmatched if question => None,
            Gated::Unmatched => Some(allow(permission)),
        };

        // Register first so the ledger's own cancellation finds the request where it must.
        self.pending_approvals.lock().await.push(*pending_request);
        if !self
            .open_request(request_id.clone(), Some(call_id.clone()), title.clone())
            .await
        {
            return;
        }

        if let Some(outcome) = auto_outcome {
            debug!(target: "acp_event_flow", call_id = %call_id, "Answering request without asking");
            if let Some(pending) = self.take_pending_request(&request_id).await {
                let mut response = acp::RequestPermissionResponse::new(outcome);
                response.meta = answer_meta;
                let _ = pending
                    .request
                    .response_tx
                    .send(Ok(acp::ClientResponse::RequestPermissionResponse(response)));
            }
            self.apply_session_event(InboundEvent::RequestAnswered { request_id })
                .await;
            return;
        }

        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(public_event))
            .await;
        self.user_notifier
            .notify(&crate::UserNotification::AwaitingApproval {
                call_id,
                command: title,
                cwd: self.cwd.display().to_string(),
            });
    }

    /// Register a request the agent is waiting on with the ledger, and perform whatever the
    /// ledger decides at once. Returns whether the request is still open afterwards.
    async fn open_request(
        &self,
        request_id: String,
        call_id: Option<String>,
        title: String,
    ) -> bool {
        let actions = self
            .session_driver
            .lock()
            .await
            .apply(InboundEvent::PermissionRequest {
                request_id: request_id.clone(),
                call_id,
                title,
            });
        let declined = actions.side_effects.iter().any(|effect| {
            matches!(
                effect,
                super::super::session_reducer::SideEffect::ResolvePermissionCancelled { request_id: declined }
                    if *declined == request_id
            )
        });
        self.dispatch_reducer_actions(actions, self.orchestrator.admission_generation())
            .await;
        !declined
    }

    async fn take_pending_request(&self, request_id: &str) -> Option<PendingApprovalRequest> {
        let mut pending = self.pending_approvals.lock().await;
        let index = pending
            .iter()
            .position(|pending| pending.request.request_id.to_string() == request_id)?;
        Some(pending.remove(index))
    }

    /// Answer a request as cancelled because the ledger's turn was cancelled or the connection
    /// died. A permission gets the explicit outcome; a question's responder is dropped. Either
    /// way the request is withdrawn from the client, so nothing goes on offering the user a
    /// question the agent was already told is cancelled.
    pub(super) async fn resolve_cancelled_permission(&self, request_id: &str) {
        let Some(pending) = self.take_pending_request(request_id).await else {
            return;
        };
        let wire_request_id = pending.request.request_id.clone();
        if matches!(
            pending.request.request,
            acp::AgentRequest::RequestPermissionRequest(_)
        ) {
            let _ = pending.request.response_tx.send(Ok(
                acp::ClientResponse::RequestPermissionResponse(
                    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
                ),
            ));
        }
        let _ = self
            .backend_event_tx
            .send(BackendEvent::Public(SessionEvent::Nori(
                nori_protocol::NoriEvent::RequestWithdrawn(wire_request_id),
            )))
            .await;
    }

    /// Every request the agent is still waiting on, by wire request ID.
    pub(in crate::backend) async fn live_request_ids(&self) -> std::collections::BTreeSet<String> {
        self.pending_approvals
            .lock()
            .await
            .iter()
            .map(|pending| pending.request.request_id.to_string())
            .collect()
    }
}

/// The answer that lets a tool run: allowing it this once where the agent offers that (so no
/// agent writes a standing rule of its own), else always; a request that offers no way to allow
/// is cancelled.
fn allow(permission: &acp::RequestPermissionRequest) -> acp::RequestPermissionOutcome {
    let pick = |kind: acp::PermissionOptionKind| {
        permission.options.iter().find(|option| option.kind == kind)
    };
    match pick(acp::PermissionOptionKind::AllowOnce)
        .or_else(|| pick(acp::PermissionOptionKind::AllowAlways))
    {
        Some(option) => acp::RequestPermissionOutcome::Selected(
            acp::SelectedPermissionOutcome::new(option.option_id.clone()),
        ),
        None => acp::RequestPermissionOutcome::Cancelled,
    }
}
