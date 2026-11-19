//! How the driver answers permission requests.

use std::sync::Arc;

use nori_protocol::acp::v1 as acp;

/// What the person decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// The first option of an allow kind.
    Allow,
    /// The first option of a reject kind.
    Deny,
    /// Withdraw the question, as cancelling the turn does.
    Cancel,
}

/// A permission request waiting for an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingPermission {
    pub request_id: acp::RequestId,
    pub session_id: acp::SessionId,
    pub tool: String,
    pub options: Vec<(String, acp::PermissionOptionKind)>,
    pub seq: i64,
}

/// Decides one permission request; `None` leaves it pending for the test.
pub type Rule = Arc<dyn Fn(&PendingPermission) -> Option<Decision> + Send + Sync>;

/// Who answers permission requests.
#[derive(Clone, Default)]
pub enum Approvals {
    /// Nobody until the test says (`approve`, `deny`, `answer`). Requests wait in
    /// `pending_permissions`; settling reports a session that waits on one.
    #[default]
    Manual,
    /// Nori's own `approval_policy = never`: the harness approves, and the client is never asked.
    AllowAll,
    /// The client is asked and the driver denies every request at once.
    DenyAll,
    /// The driver decides per request; `None` leaves it pending for the test.
    Script(Rule),
}

impl std::fmt::Debug for Approvals {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => formatter.write_str("Manual"),
            Self::AllowAll => formatter.write_str("AllowAll"),
            Self::DenyAll => formatter.write_str("DenyAll"),
            Self::Script(_) => formatter.write_str("Script(..)"),
        }
    }
}

impl Approvals {
    pub(crate) fn decide(&self, request: &PendingPermission) -> Option<Decision> {
        match self {
            Self::Manual | Self::AllowAll => None,
            Self::DenyAll => Some(Decision::Deny),
            Self::Script(decide) => decide(request),
        }
    }
}

/// The outcome of answering `request` with `decision`, or `None` when it offers no such option.
pub(crate) fn outcome(
    request: &PendingPermission,
    decision: Decision,
) -> Option<acp::RequestPermissionOutcome> {
    let wanted = |kind: acp::PermissionOptionKind| match decision {
        Decision::Allow => matches!(
            kind,
            acp::PermissionOptionKind::AllowOnce | acp::PermissionOptionKind::AllowAlways
        ),
        Decision::Deny => matches!(
            kind,
            acp::PermissionOptionKind::RejectOnce | acp::PermissionOptionKind::RejectAlways
        ),
        Decision::Cancel => false,
    };
    match decision {
        Decision::Cancel => Some(acp::RequestPermissionOutcome::Cancelled),
        Decision::Allow | Decision::Deny => request
            .options
            .iter()
            .find(|(_, kind)| wanted(*kind))
            .map(|(id, _)| {
                acp::RequestPermissionOutcome::Selected(acp::SelectedPermissionOutcome::new(
                    id.clone(),
                ))
            }),
    }
}
