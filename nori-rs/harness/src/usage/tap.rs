//! Turns the usage an agent reports into samples.
//!
//! The tap watches a session's updates (and its children's). A usage update or session-info
//! update with `_meta.nori` becomes a [`UsageSample`] holding only what is new: reported deltas
//! pass through, cumulative reports and ACP's cumulative cost are differenced against the
//! session's last report. Rate-limit windows are recorded only when they changed, so the log
//! grows with usage, not with update frequency.

use std::collections::HashMap;
use std::path::Path;

use chrono::SecondsFormat;
use chrono::Utc;
use nori_protocol::RateLimitWindow;
use nori_protocol::TokensScope;
use nori_protocol::UsageReport;
use nori_protocol::UsageTokens;
use nori_protocol::acp::v1 as acp;

use super::sample::UsageSample;
use super::sample::provider_for;
use super::store::UsageStore;
use super::store::UsageWriter;

#[derive(Debug, Default)]
struct SessionState {
    model: Option<String>,
    account: Option<String>,
    /// The last cumulative token counts a session-scoped report carried.
    cumulative: UsageTokens,
    /// The last cumulative cost ACP reported, in USD.
    cost_usd: f64,
    windows: Vec<RateLimitWindow>,
}

/// One per root session. Not `Clone`: it owns the differencing state.
#[derive(Debug)]
pub struct UsageTap {
    writer: UsageWriter,
    agent: String,
    /// The root session the last observed update belonged to or descended from.
    root_session_id: String,
    /// The delegated actor this session is, when it is one.
    actor: Option<String>,
    sessions: HashMap<String, SessionState>,
}

impl UsageTap {
    /// Must be called inside a Tokio runtime (the writer spawns its appender).
    pub fn new(nori_home: &Path, agent: &str) -> Self {
        Self {
            writer: UsageWriter::spawn(UsageStore::new(nori_home)),
            agent: agent.to_owned(),
            root_session_id: String::new(),
            actor: None,
            sessions: HashMap::new(),
        }
    }

    /// Tag every sample with the delegated actor this session is.
    #[must_use]
    pub fn for_actor(mut self, actor: Option<String>) -> Self {
        self.actor = actor;
        self
    }

    /// An update of the root session `session_id` (its id changes when `/compact` replaces it).
    pub fn observe_root(&mut self, session_id: &str, update: &acp::SessionUpdate) {
        session_id.clone_into(&mut self.root_session_id);
        self.observe(session_id, update);
    }

    /// An update of a subagent's session.
    pub fn observe_child(&mut self, session_id: &str, update: &acp::SessionUpdate) {
        self.observe(session_id, update);
    }

    fn observe(&mut self, session_id: &str, update: &acp::SessionUpdate) {
        let (meta, cost) = match update {
            acp::SessionUpdate::UsageUpdate(usage) => (usage.meta.as_ref(), usage.cost.as_ref()),
            acp::SessionUpdate::SessionInfoUpdate(info) => (info.meta.as_ref(), None),
            _ => return,
        };
        let report = meta.and_then(UsageReport::from_meta);
        let cost_usd = cost
            .filter(|cost| cost.currency.eq_ignore_ascii_case("USD"))
            .map(|cost| cost.amount);
        if report.is_none() && cost_usd.is_none() {
            return;
        }
        let report = report.unwrap_or_default();
        let state = self.sessions.entry(session_id.to_owned()).or_default();

        if let Some(model) = &report.model {
            state.model = Some(model.clone());
        }
        if let Some(account) = &report.account {
            state.account = Some(account.clone());
        }
        let tokens = match report.tokens_scope {
            TokensScope::Delta => report.tokens,
            TokensScope::Session => {
                let delta = report.tokens.delta_since(&state.cumulative);
                state.cumulative = report.tokens;
                delta
            }
        };
        let cost_delta = cost_usd.map(|total| {
            let delta = if total >= state.cost_usd {
                total - state.cost_usd
            } else {
                total
            };
            state.cost_usd = total;
            delta
        });
        let windows_changed = !report.rate_limits.is_empty() && report.rate_limits != state.windows;
        if windows_changed {
            state.windows.clone_from(&report.rate_limits);
        }
        let cost_usd = cost_delta.filter(|delta| *delta > 0.0);
        if tokens.is_empty() && cost_usd.is_none() && !windows_changed {
            return;
        }

        let model = state.model.clone().unwrap_or_else(|| "unknown".to_owned());
        let is_child = session_id != self.root_session_id;
        self.writer.record(UsageSample {
            ts: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            session_id: session_id.to_owned(),
            root_session_id: is_child.then(|| self.root_session_id.clone()),
            actor: self.actor.clone(),
            agent: self.agent.clone(),
            provider: provider_for(&self.agent, &model),
            model,
            tokens,
            cost_usd,
            account: state.account.clone(),
            rate_limits: if windows_changed {
                report.rate_limits
            } else {
                Vec::new()
            },
        });
    }
}
