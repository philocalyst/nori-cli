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

    /// The status could not be determined: the check failed or does not exist. Never a login.
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self {
            kind: "unknown".to_owned(),
            label: "Unknown".to_owned(),
            detail: Some(reason.into()),
            account: None,
        }
    }

    /// What `claude auth status --json` says about the account its config directory holds.
    /// `None` when the answer is not the shape Claude's CLI gives.
    pub fn from_claude_cli(json: &Value) -> Option<Self> {
        let text = |key: &str| json.get(key).and_then(Value::as_str).map(str::to_owned);
        let logged_in = json.get("loggedIn")?.as_bool()?;
        let api_key = text("apiKeySource");
        if !logged_in && api_key.is_none() {
            return Some(Self::signed_out());
        }
        if let Some(source) = api_key {
            return Some(Self {
                kind: "api_key".to_owned(),
                label: "Anthropic API key".to_owned(),
                detail: Some(source),
                account: None,
            });
        }
        let (email, organization, plan) =
            (text("email"), text("orgName"), text("subscriptionType"));
        if email.is_none() && plan.is_none() {
            // A login with no identity of its own: a long-lived token, a gateway.
            return Some(Self {
                kind: "external".to_owned(),
                label: match text("authMethod").as_deref() {
                    Some("oauth_token") => "Claude long-lived token".to_owned(),
                    Some(method) => format!("Claude ({method})"),
                    None => "Signed in".to_owned(),
                },
                detail: None,
                account: None,
            });
        }
        let label = match &plan {
            Some(plan) => format!("Claude {plan}"),
            None => "Claude account".to_owned(),
        };
        Some(Self::account(
            label,
            AuthAccount {
                email,
                organization,
                plan,
            },
        ))
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

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    #[test]
    fn claude_subscription_login_names_the_account() {
        let status = AuthStatus::from_claude_cli(&json!({
            "loggedIn": true,
            "authMethod": "claude.ai",
            "email": "f@example.org",
            "orgName": "F's Organization",
            "subscriptionType": "max",
        }));
        assert_eq!(
            status,
            Some(AuthStatus::account(
                "Claude max",
                AuthAccount {
                    email: Some("f@example.org".to_owned()),
                    organization: Some("F's Organization".to_owned()),
                    plan: Some("max".to_owned()),
                },
            ))
        );
    }

    #[test]
    fn claude_logged_out_is_signed_out() {
        let status =
            AuthStatus::from_claude_cli(&json!({ "loggedIn": false, "authMethod": "none" }));
        assert_eq!(status, Some(AuthStatus::signed_out()));
    }

    #[test]
    fn claude_console_login_without_a_plan_is_still_an_account() {
        let status = AuthStatus::from_claude_cli(&json!({
            "loggedIn": true,
            "authMethod": "console",
            "email": "f@example.org",
        }));
        assert_eq!(
            status.map(|status| (status.kind, status.label)),
            Some(("account".to_owned(), "Claude account".to_owned()))
        );
    }

    #[test]
    fn claude_token_login_has_no_identity() {
        let status =
            AuthStatus::from_claude_cli(&json!({ "loggedIn": true, "authMethod": "oauth_token" }));
        assert_eq!(
            status.map(|status| (status.kind, status.label)),
            Some(("external".to_owned(), "Claude long-lived token".to_owned()))
        );
    }

    #[test]
    fn claude_answer_of_another_shape_is_not_a_status() {
        assert_eq!(AuthStatus::from_claude_cli(&json!({ "hello": 1 })), None);
    }
}
