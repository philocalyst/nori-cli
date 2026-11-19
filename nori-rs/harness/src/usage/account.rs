//! Whose quota a sample spent: a provider and an account, never an agent name or a model alone.
//!
//! An account is what a provider's windows belong to. Its identity, best source first:
//!
//! 1. the `_meta.nori.account` the agent reported (an email, a workspace id);
//! 2. the last account the same agent reported (so a report that omits it still lands on it);
//! 3. the agent's own slug, which names its profile: `claude-code` is Claude "primary",
//!    `claude2` is Claude "secondary".
//!
//! A gateway (`opencode`) is billed by the provider of the model it routed to, so its samples
//! belong to that provider under the gateway's own account, shown as "via opencode". The same
//! tokens are never counted twice when a gateway and a native agent both report them.

use std::collections::HashMap;

use chrono::Duration;

use super::sample::UsageSample;
use crate::recovery::is_gateway;

/// One account of one provider.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AccountId {
    pub provider: String,
    /// The gateway the tokens went through, when they did. Accounts reached directly sort first.
    pub via: Option<String>,
    /// Where the agent's profile sorts among the provider's accounts (the slug's number: 1 for
    /// `claude-code`, 2 for `claude2`).
    pub ordinal: i64,
    /// What groups samples: the reported account, or the agent that stands for it.
    pub key: String,
    /// What the view shows: the reported account, `primary`, `secondary`, or `via opencode`.
    pub label: String,
    /// The agents that reported on this account.
    pub agents: Vec<String>,
}

impl AccountId {
    /// The account an agent's samples fall on when it never reported one.
    pub fn derived(agent: &str, provider: &str) -> Self {
        Self::resolve(agent, provider, None)
    }

    fn resolve(agent: &str, provider: &str, reported: Option<&str>) -> Self {
        let (stem, number) = split_slug(agent);
        let gateway = is_gateway(stem).then(|| agent.to_owned());
        let (key, label) = match (&gateway, reported) {
            (Some(gateway), Some(account)) => (format!("{gateway}:{account}"), account.to_owned()),
            (Some(gateway), None) => (format!("via:{gateway}"), format!("via {gateway}")),
            (None, Some(account)) => (account.to_owned(), account.to_owned()),
            // A native account (`codex@work`) is named by the user, not numbered.
            (None, None) => (
                format!("agent:{agent}"),
                agent
                    .split_once('@')
                    .map_or_else(|| ordinal_label(number), |(_, name)| name.to_owned()),
            ),
        };
        Self {
            provider: provider.to_owned(),
            via: gateway,
            ordinal: number,
            key,
            label,
            agents: vec![agent.to_owned()],
        }
    }

    /// The account as one phrase: `primary`, `secondary`, `you@example.com via opencode`.
    pub fn describe(&self) -> String {
        match &self.via {
            Some(via) if !self.label.starts_with("via ") => format!("{} via {via}", self.label),
            Some(_) | None => self.label.clone(),
        }
    }

    pub(super) fn merge_agents(&mut self, agents: &[String]) {
        for agent in agents {
            if !self.agents.contains(agent) {
                self.agents.push(agent.clone());
            }
        }
    }
}

/// A slug without its trailing number, and the number (1 when it has none). A native account's
/// slug (`codex@work`) is the provider, then the account's name.
fn split_slug(agent: &str) -> (&str, i64) {
    if let Some((provider, _)) = agent.split_once('@') {
        return (provider, 1);
    }
    let stem = agent.trim_end_matches(|c: char| c.is_ascii_digit());
    let digits = &agent[stem.len()..];
    match digits.parse::<i64>() {
        Ok(number) if !stem.is_empty() && number > 0 => (stem.trim_end_matches(['-', '_']), number),
        Ok(_) | Err(_) => (agent, 1),
    }
}

fn ordinal_label(number: i64) -> String {
    match number {
        1 => "primary".to_owned(),
        2 => "secondary".to_owned(),
        3 => "tertiary".to_owned(),
        other => format!("#{other}"),
    }
}

/// Maps samples to accounts, remembering what each agent last said its account was.
#[derive(Debug, Default)]
pub struct AccountResolver {
    reported: HashMap<String, String>,
}

impl AccountResolver {
    /// `samples` in any order; the newest report of each agent wins.
    pub fn new(samples: &[UsageSample]) -> Self {
        let mut newest: HashMap<&str, (&str, chrono::DateTime<chrono::Utc>)> = HashMap::new();
        for sample in samples {
            let (Some(account), Some(time)) = (sample.account.as_deref(), sample.time()) else {
                continue;
            };
            match newest.get(sample.agent.as_str()) {
                Some((_, seen)) if *seen >= time => {}
                _ => {
                    newest.insert(sample.agent.as_str(), (account, time));
                }
            }
        }
        Self {
            reported: newest
                .into_iter()
                .map(|(agent, (account, _))| (agent.to_owned(), account.to_owned()))
                .collect(),
        }
    }

    pub fn resolve(&self, sample: &UsageSample) -> AccountId {
        let reported = sample
            .account
            .as_deref()
            .or_else(|| self.reported.get(&sample.agent).map(String::as_str));
        AccountId::resolve(&sample.agent, &sample.provider, reported)
    }
}

/// Two accounts of one provider that ended up with the same label (`claude` and `claude-code`
/// are both "primary") are told apart by the agent that stands for each.
pub(super) fn disambiguate(accounts: &mut [AccountId]) {
    let labels: Vec<(String, String)> = accounts
        .iter()
        .map(|account| (account.provider.clone(), account.label.clone()))
        .collect();
    for (index, account) in accounts.iter_mut().enumerate() {
        let twins = labels
            .iter()
            .filter(|label| **label == labels[index])
            .count();
        if twins > 1
            && let Some(agent) = account.agents.first()
        {
            account.label = format!("{} ({agent})", account.label);
        }
    }
}

/// When a native agent reported tokens, and how many.
type Report = (chrono::DateTime<chrono::Utc>, i64);

/// Two samples this close in time with the same tokens are one report seen twice.
const ECHO_WINDOW_SECONDS: i64 = 10;

/// Drop the gateway's copy of tokens a native agent reported too.
///
/// A gateway that fronts a native agent (or one that relays a delegated child's report) can
/// state the same tokens the native session already recorded. Such a sample shares its root
/// session with the native one, names the same provider, and carries identical tokens within
/// moments; the native agent's is the report that counts.
pub(super) fn without_gateway_echoes(samples: Vec<UsageSample>) -> Vec<UsageSample> {
    let root = |sample: &UsageSample| {
        sample
            .root_session_id
            .clone()
            .unwrap_or_else(|| sample.session_id.clone())
    };
    let mut native: HashMap<(String, String), Vec<Report>> = HashMap::new();
    for sample in &samples {
        if is_gateway(split_slug(&sample.agent).0) || sample.tokens.is_empty() {
            continue;
        }
        if let Some(time) = sample.time() {
            native
                .entry((root(sample), sample.provider.clone()))
                .or_default()
                .push((time, sample.tokens.total()));
        }
    }
    samples
        .into_iter()
        .filter(|sample| {
            if !is_gateway(split_slug(&sample.agent).0) || sample.tokens.is_empty() {
                return true;
            }
            let Some(time) = sample.time() else {
                return true;
            };
            let echoed = native
                .get(&(root(sample), sample.provider.clone()))
                .is_some_and(|reports| {
                    reports.iter().any(|(seen, total)| {
                        *total == sample.tokens.total()
                            && (*seen - time).abs() <= Duration::seconds(ECHO_WINDOW_SECONDS)
                    })
                });
            !echoed
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use nori_protocol::UsageTokens;
    use pretty_assertions::assert_eq;

    use super::*;

    fn sample(agent: &str, provider: &str, ts: &str, session: &str, total: i64) -> UsageSample {
        UsageSample {
            ts: ts.to_owned(),
            session_id: session.to_owned(),
            root_session_id: None,
            actor: None,
            agent: agent.to_owned(),
            provider: provider.to_owned(),
            model: "m".to_owned(),
            tokens: UsageTokens {
                input: total,
                ..UsageTokens::default()
            },
            cost_usd: None,
            account: None,
            rate_limits: Vec::new(),
        }
    }

    #[test]
    fn profiles_of_one_provider_are_different_accounts() {
        let first = AccountId::derived("claude-code", "claude");
        let second = AccountId::derived("claude2", "claude");
        assert_eq!(
            (first.label.as_str(), second.label.as_str()),
            ("primary", "secondary")
        );
        assert!(first.key != second.key);
        assert!(first < second);
    }

    #[test]
    fn a_gateway_is_its_own_account_of_the_upstream_provider() {
        let routed = AccountId::derived("opencode", "deepseek");
        assert_eq!(routed.provider, "deepseek");
        assert_eq!(routed.via.as_deref(), Some("opencode"));
        assert_eq!(routed.describe(), "via opencode");
    }

    #[test]
    fn a_report_without_an_account_lands_on_the_one_the_agent_named_before() {
        let mut named = sample("claude-code", "claude", "2026-11-18T10:00:00.000Z", "a", 5);
        named.account = Some("me@example.com".to_owned());
        let bare = sample("claude-code", "claude", "2026-11-18T11:00:00.000Z", "b", 5);
        let resolver = AccountResolver::new(&[named, bare.clone()]);
        assert_eq!(resolver.resolve(&bare).key, "me@example.com");
    }

    #[test]
    fn colliding_labels_name_their_agent() {
        let mut accounts = vec![
            AccountId::derived("claude", "claude"),
            AccountId::derived("claude-code", "claude"),
            AccountId::derived("codex", "codex"),
        ];
        disambiguate(&mut accounts);
        assert_eq!(
            accounts
                .iter()
                .map(|account| account.label.as_str())
                .collect::<Vec<_>>(),
            ["primary (claude)", "primary (claude-code)", "primary"]
        );
    }

    #[test]
    fn a_gateway_echo_of_native_tokens_is_not_counted_twice() {
        let mut native = sample(
            "claude-code",
            "claude",
            "2026-11-18T10:00:00.000Z",
            "child",
            700,
        );
        native.root_session_id = Some("root".to_owned());
        let echo = sample(
            "opencode",
            "claude",
            "2026-11-18T10:00:02.000Z",
            "root",
            700,
        );
        let other = sample(
            "opencode",
            "claude",
            "2026-11-18T10:05:00.000Z",
            "root",
            700,
        );
        let kept = without_gateway_echoes(vec![native.clone(), echo, other.clone()]);
        assert_eq!(kept, vec![native, other]);
    }
}
