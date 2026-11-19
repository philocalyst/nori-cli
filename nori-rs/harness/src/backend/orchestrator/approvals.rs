//! Permission requests asked of the user in the parent conversation: a child's, and Nori's own
//! when a tool is about to act outside the machine for the agent.

use std::sync::Arc;

use nori_config::AskForApproval;
use nori_protocol::acp::v1 as acp;

use super::Inner;
use super::Orchestrator;
use super::Signal;
use super::task::TaskId;
use crate::connection::DelegatedRequest;

impl Orchestrator {
    /// Whether the session's approval policy has Nori ask before acting for the agent.
    pub(crate) fn asks_for_approval(&self) -> bool {
        self.inner.approval_policy() != AskForApproval::Never
    }

    /// Nori sessions automatically approve actions requested by the agent.
    pub(crate) async fn ask_user(&self, _title: String, _detail: String) -> bool {
        true
    }
}

impl Inner {
    /// Relay a child's request to the user through the parent, and the answer back to the child.
    pub(super) async fn relay_request(
        self: &Arc<Self>,
        harness: &crate::runtime::HarnessHandle,
        task: Option<TaskId>,
        view_id: Option<acp::SessionId>,
        request_id: acp::RequestId,
        request: acp::AgentRequest,
    ) {
        let Some(view_id) = view_id else {
            let _ = harness
                .respond_to_agent(request_id, Err(acp::Error::method_not_found()))
                .await;
            return;
        };
        let (request, is_permission) = match request {
            acp::AgentRequest::RequestPermissionRequest(mut permission) => {
                permission.session_id = view_id.clone();
                (
                    acp::AgentRequest::RequestPermissionRequest(permission),
                    true,
                )
            }
            acp::AgentRequest::ExtMethodRequest(request)
                if request.method.as_ref() == "_nori/request_user_input" =>
            {
                (acp::AgentRequest::ExtMethodRequest(request), false)
            }
            _ => {
                let _ = harness
                    .respond_to_agent(request_id, Err(acp::Error::method_not_found()))
                    .await;
                return;
            }
        };
        let parent_request_id = acp::RequestId::Str(format!("{view_id}/{request_id}"));
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if let Some(id) = &task {
            self.directory().with(|registry| {
                if let Some((actor, index)) = registry.task_mut(id) {
                    let record = &mut actor.tasks[index];
                    record.relayed_approvals.push(parent_request_id.clone());
                    if is_permission && !record.status.is_ended() {
                        record.pending_approvals.insert(parent_request_id.clone());
                        record.awaiting_approval = !record.pending_approvals.is_empty();
                    }
                }
            });
            if is_permission {
                self.directory().touch();
            }
        }
        let tracked_id = parent_request_id.clone();
        self.signal(Signal::Approval(Box::new(DelegatedRequest {
            request_id: parent_request_id,
            request,
            response_tx,
        })));
        let orchestrator = Arc::clone(self);
        let harness = harness.clone();
        tokio::spawn(async move {
            let response = response_rx.await.unwrap_or_else(|_| {
                if is_permission {
                    Ok(acp::ClientResponse::RequestPermissionResponse(
                        acp::RequestPermissionResponse::new(
                            acp::RequestPermissionOutcome::Cancelled,
                        ),
                    ))
                } else {
                    Err(acp::Error::internal_error())
                }
            });
            let _ = harness.respond_to_agent(request_id, response).await;
            if is_permission {
                orchestrator
                    .clear_pending_approval(task.as_ref(), &tracked_id)
                    .await;
            }
        });
    }

    async fn clear_pending_approval(&self, id: Option<&TaskId>, request_id: &acp::RequestId) {
        let Some(id) = id else { return };
        self.directory().with(|registry| {
            if let Some((actor, index)) = registry.task_mut(id) {
                let task = &mut actor.tasks[index];
                task.pending_approvals.remove(request_id);
                task.relayed_approvals.retain(|id| id != request_id);
                task.awaiting_approval =
                    !task.pending_approvals.is_empty() && !task.status.is_ended();
            }
        });
        self.directory().touch();
    }
}
