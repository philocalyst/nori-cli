//! Who an agent is signed in as: the `_auth/status_update` notification.
//!
//! The Claude adapter pushes it (see the `authStatus` extension of `claude-agent-acp`); an agent
//! built on this kit sends the same notification, so the host reads one shape from every agent. It
//! says which identity pays for the agent's turns, never the credential: a kind, a label, and for
//! a subscription login the email, the organization and the plan.

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use crate::acp;
use crate::acp::UntypedMessage;
use crate::sink::Cx;

/// The notification's method.
pub const AUTH_STATUS_UPDATE: &str = "_auth/status_update";

/// The identity behind an agent's login.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthStatus {
    /// `account` (a subscription login), `api_key`, `gateway`, `external` or `none`.
    pub kind: String,
    /// How a person reads it: `Claude Max`, `Anthropic API key`, `Not logged in`.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<AuthAccount>,
}

/// The subscription behind an `account` login.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthAccount {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl AuthStatus {
    /// A subscription login.
    pub fn account(label: impl Into<String>, account: AuthAccount) -> Self {
        Self {
            kind: "account".to_owned(),
            label: label.into(),
            detail: None,
            account: Some(account),
        }
    }

    /// Not signed in.
    pub fn signed_out() -> Self {
        Self {
            kind: "none".to_owned(),
            label: "Not logged in".to_owned(),
            detail: None,
            account: None,
        }
    }

    /// The status in a notification's params (`{ "authStatus": { ... } }`), when it holds one.
    pub fn from_params(params: &Value) -> Option<Self> {
        serde_json::from_value(params.get("authStatus")?.clone()).ok()
    }
}

/// Tell the client who the agent is signed in as.
pub fn announce_auth_status(cx: &Cx, status: &AuthStatus) -> acp::Result<()> {
    let params = serde_json::json!({ "authStatus": status });
    cx.send_notification(UntypedMessage::new(AUTH_STATUS_UPDATE, params)?)
}
