//! Which ACP session an inbound event belongs to.
//!
//! A swap (compaction, fork, rewind) replaces the active session on the same connection. The
//! old session is retired, and the agent may still send it updates or ask it for permissions.
//! None of that may reach the new session's ledger: a stale tool update would open a turn that
//! nothing can finish, and a stale permission request would be answered as if it were the
//! conversation's. Each inbound event is scoped by its ACP session ID at the relay, the driver's
//! boundary. Events of retired sessions are dropped and logged once per session; permission
//! requests of a retired session are declined explicitly, with a notice.
//!
//! Only retired sessions are refused, not "anything but the active one": the first updates of a
//! new session can arrive before the swap has installed its ID.

use std::collections::HashMap;
use std::collections::HashSet;

use nori_protocol::AcpEvent;

use super::*;
use crate::connection::DelegatedRequest;

#[derive(Default)]
pub(super) struct RetiredSessions {
    /// Retired session IDs, with whether the first drop was already logged.
    ids: std::sync::Mutex<HashMap<String, bool>>,
}

impl RetiredSessions {
    pub(super) fn retire(&self, session: &acp::SessionId) {
        self.ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(session.to_string())
            .or_insert(false);
    }

    /// How an event of `session` is treated. The first refusal for a session is logged.
    fn refusal(&self, session: &acp::SessionId, what: &str) -> Refusal {
        let mut ids = self
            .ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(logged) = ids.get_mut(&session.to_string()) else {
            return Refusal::None;
        };
        if std::mem::replace(logged, true) {
            return Refusal::Repeat;
        }
        tracing::warn!(
            %session,
            what,
            "dropping events of a session that was swapped away (logged once per session)"
        );
        Refusal::First
    }
}

#[derive(PartialEq, Eq)]
enum Refusal {
    /// The session is not retired.
    None,
    First,
    Repeat,
}

impl AcpBackend {
    /// Retire the session a swap replaced. Permission requests it still holds are declined
    /// explicitly, and the user is told.
    pub(super) async fn retire_session(&self, old: &acp::SessionId) {
        self.retired_sessions.retire(old);
        let held = {
            let mut pending = self.pending_approvals.lock().await;
            let (retired, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
                .into_iter()
                .partition(|pending| {
                    matches!(
                        &pending.request.request,
                        acp::AgentRequest::RequestPermissionRequest(request)
                            if request.session_id == *old
                    )
                });
            *pending = kept;
            retired
        };
        if held.is_empty() {
            return;
        }
        let mut tools = HashSet::new();
        for pending in held {
            if let acp::AgentRequest::RequestPermissionRequest(request) = &pending.request.request {
                tools.insert(
                    request
                        .tool_call
                        .fields
                        .title
                        .clone()
                        .unwrap_or_else(|| "a tool".to_owned()),
                );
            }
            let _ = pending.request.response_tx.send(Ok(
                acp::ClientResponse::RequestPermissionResponse(
                    acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
                ),
            ));
            let _ = self
                .backend_event_tx
                .send(BackendEvent::Public(SessionEvent::Nori(
                    nori_protocol::NoriEvent::RequestWithdrawn(pending.request.request_id.clone()),
                )))
                .await;
        }
        let mut tools = tools.into_iter().collect::<Vec<_>>();
        tools.sort();
        self.notice(&format!(
            "The session was replaced while the agent still waited to {}; those requests were declined.",
            tools.join(", ")
        ))
        .await;
    }

    /// Whether `event` is traffic of a retired session, which must not reach the ledger. A
    /// root notification of one is followed by its private reducer input, which the caller
    /// must drop with it.
    pub(super) fn drops_retired(&self, event: &AcpEvent) -> bool {
        let AcpEvent::Notification(acp::AgentNotification::SessionNotification(notification)) =
            event
        else {
            return false;
        };
        self.retired_sessions
            .refusal(&notification.session_id, "session/update")
            != Refusal::None
    }

    /// Decline a permission request of a retired session. Anything else is handed back.
    pub(super) async fn decline_retired_request(
        &self,
        request: DelegatedRequest,
    ) -> Option<DelegatedRequest> {
        let acp::AgentRequest::RequestPermissionRequest(permission) = &request.request else {
            return Some(request);
        };
        let refusal = self
            .retired_sessions
            .refusal(&permission.session_id, "session/request_permission");
        if refusal == Refusal::None {
            return Some(request);
        }
        let tool = permission
            .tool_call
            .fields
            .title
            .clone()
            .unwrap_or_else(|| "a tool".to_owned());
        let _ = request
            .response_tx
            .send(Ok(acp::ClientResponse::RequestPermissionResponse(
                acp::RequestPermissionResponse::new(acp::RequestPermissionOutcome::Cancelled),
            )));
        if refusal == Refusal::First {
            self.notice(&format!(
                "The agent asked to {tool} from a session that was replaced; the request was declined."
            ))
            .await;
        }
        None
    }
}
