//! Who each account is signed in as, as its agent last said.
//!
//! An agent reports its identity in `_auth/status_update` (see `nori_acp_kit::AuthStatus`): an
//! email, an organization, a plan. The host files it here by the agent's slug, in memory and in
//! `<nori home>/accounts/identity.json`, so `/providers` can show "robert@example.com · Max" for an
//! account whose agent is not running. Only what the agent said is kept; a credential never
//! reaches this module.

use std::collections::BTreeMap;
use std::sync::Mutex;

use nori_acp_kit::AuthStatus;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountIdentity {
    pub kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
}

impl AccountIdentity {
    pub fn from_status(status: &AuthStatus) -> Self {
        let account = status.account.clone().unwrap_or_default();
        Self {
            kind: status.kind.clone(),
            label: status.label.clone(),
            email: account.email,
            organization: account.organization,
            plan: account.plan,
        }
    }

    /// The identity on one line: `robert@example.com · Max`, `Anthropic API key`, `signed out`.
    pub fn describe(&self) -> String {
        match (self.kind.as_str(), &self.email) {
            ("none", _) => "signed out".to_owned(),
            (_, Some(email)) => match self.plan_label() {
                Some(plan) => format!("{email} · {plan}"),
                None => email.clone(),
            },
            _ => self.label.clone(),
        }
    }

    /// The plan as a person names it: `Max` from `max` or `Claude Max`.
    fn plan_label(&self) -> Option<String> {
        let raw = self.plan.as_deref().unwrap_or(&self.label);
        let word = raw
            .split_whitespace()
            .filter(|word| !word.eq_ignore_ascii_case("claude"))
            .collect::<Vec<_>>()
            .join(" ");
        if word.is_empty() {
            return None;
        }
        let mut chars = word.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
    }
}

static IDENTITIES: Mutex<Option<BTreeMap<String, AccountIdentity>>> = Mutex::new(None);

fn path() -> Option<std::path::PathBuf> {
    nori_config::find_nori_home()
        .ok()
        .map(|home| home.join("accounts").join("identity.json"))
}

fn with_store<T>(use_store: impl FnOnce(&mut BTreeMap<String, AccountIdentity>) -> T) -> T {
    let mut guard = IDENTITIES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let store = guard.get_or_insert_with(|| {
        path()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    });
    use_store(store)
}

/// What the agent with `slug` last said about who it is signed in as.
pub fn identity_of(slug: &str) -> Option<AccountIdentity> {
    with_store(|store| store.get(slug).cloned())
}

/// File what an agent just said, and keep it for the next launch. Nothing is written when it is
/// what was already known.
pub fn record_identity(slug: &str, status: &AuthStatus) {
    // "I could not tell" says nothing about who the account is.
    if status.kind == "unknown" {
        return;
    }
    let identity = AccountIdentity::from_status(status);
    let snapshot = with_store(|store| {
        if store.get(slug) == Some(&identity) {
            return None;
        }
        store.insert(slug.to_owned(), identity);
        Some(store.clone())
    });
    let (Some(snapshot), Some(path)) = (snapshot, path()) else {
        return;
    };
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| serde_json::to_vec_pretty(&snapshot).map_err(std::io::Error::other))
        .and_then(|bytes| {
            let temporary = path.with_extension("json.tmp");
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, &path)
        });
    if let Err(error) = written {
        tracing::warn!(
            "could not save account identities to {}: {error}",
            path.display()
        );
    }
}
