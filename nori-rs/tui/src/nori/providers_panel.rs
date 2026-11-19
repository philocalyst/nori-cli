//! `/providers`: the accounts Nori can run, grouped by provider, and managing them. Models are not
//! here; they are chosen in `/model`.
//!
//! Under each provider (Claude, Codex, Gemini, …, then agents that belong to none) stand its
//! accounts: the built-in login, the `[[accounts]]` Nori runs itself and the `[[agents]]` wrappers
//! it reads as accounts (`nori_acp_host::accounts`). Each says who it is signed in as once its
//! agent has said so, how much room its tightest rate limit window has left, and how it stands:
//!
//! - **signed in** once its agent has started a session and told Nori its models;
//! - **not signed in** when it is installed but has not;
//! - **not installed** otherwise.
//!
//! Choosing an account opens its page: signing in (or again), where its login lives and what
//! starts it. A provider whose accounts Nori can run itself ends with "Add another account", which
//! asks for a name, creates the account with its own home and signs it in; one whose login Nori
//! cannot multiply says why. The last row opens the fallback order, which is Pkl
//! ([`super::fallbacks_panel`]).

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use nori_acp_host::accounts::Account;
use nori_acp_host::accounts::AccountIdentity;
use nori_acp_host::accounts::AddPlan;
use nori_acp_host::accounts::Home;
use nori_acp_host::accounts::Origin;
use nori_config::ProviderAccounts;
use nori_harness::AcpAgentInfo;
use nori_harness::recovery::AccountHeadroom;

use crate::app_event::AppEvent;
use crate::choice_picker::Ask;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;
use crate::choice_picker::Next;
use crate::choice_picker::StepAction;

/// How an account stands, best first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    SignedIn,
    SignIn,
    NotInstalled,
}

/// One account, as the panel shows it.
#[derive(Clone)]
struct Row {
    standing: Standing,
    agent: AcpAgentInfo,
    account: Option<Account>,
    identity: Option<AccountIdentity>,
    /// Its tightest rate limit window, in words.
    room: Option<String>,
}

/// The providers panel for a Nori home, with `active_agent` marked as the one in use.
pub(crate) fn spec(nori_home: &Path, active_agent: &str) -> ChoiceSpec {
    let headroom = AccountHeadroom::load(nori_home);
    // Accounts by provider, in the order the registry lists them; `None` holds the agents that
    // belong to no provider (a custom agent, a debug fixture in use).
    let mut groups: Vec<(Option<&'static ProviderAccounts>, Vec<Row>)> = Vec::new();
    for agent in nori_harness::list_available_agents() {
        // Debug fixtures are not providers, unless a session is running on one.
        if nori_harness::recovery::is_debug_fixture(&agent.agent_name)
            && !nori_harness::recovery::is_debug_fixture(active_agent)
        {
            continue;
        }
        let identity = nori_acp_host::accounts::identity_of(&agent.agent_name);
        // An agent that says it is signed out is, whatever it listed before; the agent in use is
        // signed in whether or not it lists models.
        let standing = if !agent.is_installed {
            Standing::NotInstalled
        } else if identity.as_ref().is_some_and(|who| who.kind == "none") {
            Standing::SignIn
        } else if agent.agent_name == active_agent
            || !nori_harness::cached_agent_models(nori_home, &agent.agent_name).is_empty()
        {
            Standing::SignedIn
        } else {
            Standing::SignIn
        };
        let account = nori_acp_host::accounts::find(&agent.agent_name);
        let provider = account.as_ref().map(|account| account.provider);
        let room = provider
            .and_then(|provider| headroom.room(&agent.agent_name, provider.id))
            .map(|room| {
                let resets = room.resets_at.map(|at| {
                    let minutes = (at - Utc::now()).num_minutes().max(0);
                    match minutes {
                        0..60 => format!(" · resets in {minutes}m"),
                        60..2880 => format!(" · resets in {}h", minutes / 60),
                        _ => format!(" · resets in {}d", minutes / 1440),
                    }
                });
                let left = if room.is_spent() {
                    format!("{} window spent", room.window)
                } else {
                    format!("{:.0}% of {} left", room.percent_left, room.window)
                };
                format!("{left}{}", resets.unwrap_or_default())
            });
        let row = Row {
            standing,
            identity,
            account,
            agent,
            room,
        };
        match groups.iter_mut().find(|(known, _)| *known == provider) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((provider, vec![row])),
        }
    }
    let in_use = |rows: &[Row]| rows.iter().any(|row| row.agent.agent_name == active_agent);
    for (_, rows) in &mut groups {
        rows.sort_by_key(|row| (row.agent.agent_name != active_agent, row.standing));
    }
    // The provider in use first, then the ones signed in, then the rest; agents of no provider
    // last. The sort is stable, so ties keep the registry's order.
    groups.sort_by_key(|(provider, rows)| {
        let best = rows.iter().map(|row| row.standing).min();
        (!in_use(rows), provider.is_none(), best)
    });

    let home: Arc<PathBuf> = Arc::new(nori_home.to_path_buf());
    let back: StepAction = {
        let home = home.clone();
        let active: Arc<str> = Arc::from(active_agent);
        Arc::new(move |_| Next::Show(spec(&home, &active)))
    };
    let mut items = Vec::new();
    for (provider, rows) in groups {
        let section = provider.map_or("Other agents", |provider| provider.title);
        for row in rows {
            let current = row.agent.agent_name == active_agent;
            let detail = summary(&row);
            let keywords = row.agent.agent_name.clone();
            let label = row.agent.display_name.clone();
            let back = back.clone();
            let mut item = ChoiceItem::step(
                label,
                Arc::new(move |_| Next::Show(account_spec(&row, back.clone()))),
            )
            .detail(detail)
            .section(section)
            .current(current);
            item.keywords = Some(keywords);
            items.push(item);
        }
        // A provider with one login in one place has nothing to add.
        if let Some(provider) = provider
            && provider.home.is_some()
        {
            items.push(add_account_item(provider).section(section));
        }
    }
    items.push(super::fallbacks_panel::entry(home, back).section("Routing"));
    let mut spec = ChoiceSpec::new("Providers", items);
    spec.subtitle = Some("Enter opens an account: sign in, see its login, add another".to_owned());
    spec
}

/// An account on one line: who, how much room, how it stands.
fn summary(row: &Row) -> String {
    let who = row.identity.as_ref().map(AccountIdentity::describe);
    match row.standing {
        Standing::SignedIn => [who, row.room.clone()]
            .into_iter()
            .flatten()
            .reduce(|line, part| format!("{line} · {part}"))
            .unwrap_or_else(|| "signed in".to_owned()),
        Standing::SignIn => "not signed in".to_owned(),
        Standing::NotInstalled => match &row.agent.unavailable {
            Some(reason) => reason.clone(),
            None => format!("not installed · {}", row.agent.description),
        },
    }
}

/// One account's page: signing in, and where it lives.
fn account_spec(row: &Row, back: StepAction) -> ChoiceSpec {
    let agent_name = row.agent.agent_name.clone();
    let (label, detail) = match row.standing {
        Standing::SignedIn => ("Sign in again", "renew this login"),
        Standing::SignIn => ("Sign in", "in a panel above the prompt"),
        Standing::NotInstalled => ("Install", "how to install it"),
    };
    let mut items = vec![
        ChoiceItem::run(
            label,
            Box::new(move |tx| {
                tx.send(AppEvent::LoginProvider {
                    agent_name: agent_name.clone(),
                })
            }),
        )
        .detail(detail)
        .section("Account"),
    ];
    if let Some(who) = &row.identity
        && who.kind != "none"
    {
        items.push(info("Signed in as", who.describe()));
    }
    if let Some(room) = &row.room {
        items.push(info("Room", room.clone()));
    }
    if let Some(account) = &row.account {
        let home = match &account.home {
            Home::At(path) => path.display().to_string(),
            Home::Default => format!("where {} keeps its own", account.provider.title),
        };
        items.push(info("Login lives in", home));
        let started = match &account.origin {
            Origin::Default => "the built-in agent".to_owned(),
            Origin::Native => "Nori, in its own process".to_owned(),
            Origin::Wrapper { launcher, .. } => launcher.clone(),
        };
        items.push(info("Started by", started));
        items.push(info("Agent", account.slug.clone()));
    }
    let mut page = ChoiceSpec::new(row.agent.display_name.clone(), items);
    page.subtitle = Some(summary(row));
    page.on_back = Some(back);
    page
}

/// A line of an account's page that says something and does nothing.
fn info(label: &str, detail: String) -> ChoiceItem {
    let mut item = ChoiceItem::run(label, Box::new(|_| {}))
        .detail(detail)
        .section("About");
    item.disabled = true;
    item
}

/// "Add another account" for `provider`: a name, then the account is created and signed in. When
/// this build cannot run another account of it, choosing the row says why instead.
fn add_account_item(provider: &'static ProviderAccounts) -> ChoiceItem {
    let label = format!("Add another {} account", provider.title);
    match nori_acp_host::accounts::plan_add(provider) {
        AddPlan::Native => ChoiceItem::ask(
            label,
            Ask {
                verb: "name",
                label: format!("{} account", provider.title),
                initial: String::new(),
                then: Arc::new(move |text, tx| {
                    let name = text.trim();
                    nori_config::check_account_name(name)?;
                    let slug = nori_config::account_slug(provider.id, name);
                    if nori_acp_host::accounts::find(&slug).is_some() {
                        return Err(format!(
                            "{} already has an account named {name}",
                            provider.title
                        ));
                    }
                    tx.send(AppEvent::AddAccount {
                        provider,
                        name: name.to_owned(),
                    });
                    Ok(Next::Done)
                }),
            },
        )
        .detail("its own login, beside the others"),
        AddPlan::Unavailable(reason) => ChoiceItem::run(
            label,
            Box::new(move |tx| {
                tx.send(AppEvent::InsertHistoryCell(Box::new(
                    crate::history_cell::new_info_event(
                        format!("Nori cannot add another {} account here", provider.title),
                        Some(reason.clone()),
                    ),
                )))
            }),
        )
        .detail("set up outside Nori · Enter says how"),
    }
}
