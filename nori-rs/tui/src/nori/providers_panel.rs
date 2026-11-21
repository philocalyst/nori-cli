//! `/providers`: the accounts Nori can run, grouped by provider, and managing them. Models are not
//! here; they are chosen in `/model`.
//!
//! Under each provider (Claude, Codex, Gemini, …, then agents that belong to none) stand its
//! accounts: the built-in login, the `[[accounts]]` Nori runs itself and the `[[agents]]` wrappers
//! it reads as accounts (`nori_acp_host::accounts`). Each says who it is signed in as, how much
//! room its tightest rate limit window has left, and how it stands. How it stands is found out by
//! asking the account's own login (`nori_acp_host::accounts::status`), never assumed:
//!
//! - **signed in** (as whom, when it says) once the check found the login;
//! - **not signed in** (and why) once the check found none;
//! - **checking** until the first answer lands: opening the panel starts one check per account on
//!   a task, and each answer refreshes the row ([`AccountChecked`]);
//! - **can't tell** when the check could not run, or opened no proof (an Antigravity session that
//!   lists no models); a check that is running again says **checking** over the verdict of the
//!   last one, never the old verdict as if it were current;
//! - **not installed** when there is nothing to run, with the one command that installs it (the
//!   Gemini CLI is looked for in the usual install locations, not only on `PATH`, and its page says
//!   what else signs in without it).
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
use nori_acp_host::accounts::AccountStatus;
use nori_acp_host::accounts::AddPlan;
use nori_acp_host::accounts::Home;
use nori_acp_host::accounts::Origin;
use nori_acp_host::registry::AgentKind;
use nori_config::ProviderAccounts;
use nori_harness::AcpAgentInfo;
use nori_harness::recovery::AccountHeadroom;

use crate::app_event::AppEvent;
use crate::choice_picker::Ask;
use crate::choice_picker::ChoiceItem;
use crate::choice_picker::ChoiceSpec;
use crate::choice_picker::Next;
use crate::choice_picker::StepAction;

/// Told to the open panel when a check of the account with this slug landed.
pub(crate) struct AccountChecked(pub(crate) String);

/// How an account stands, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Standing {
    SignedIn,
    Checking,
    SignIn,
    Unknown,
    NotInstalled,
}

/// One account, as the panel shows it.
#[derive(Clone)]
struct Row {
    standing: Standing,
    /// Why it stands there, when its check said (why it is not signed in, why it is unknown).
    reason: Option<String>,
    agent: AcpAgentInfo,
    account: Option<Account>,
    identity: Option<AccountIdentity>,
    /// Its tightest rate limit window, in words.
    room: Option<String>,
    /// While it is not installed: the one command that installs it.
    install: Option<String>,
    /// While it is not installed: what else works.
    instead: Option<String>,
}

/// Where the panel is, so a refreshed row can rebuild the pages around it.
#[derive(Clone)]
struct Place {
    home: Arc<PathBuf>,
    active: Arc<str>,
}

/// Start a check of every account whose login Nori can verify, on tasks, and mark them checking
/// so the panel drawn next already says so. Each answer is announced as `AccountChecked`, and a
/// frame is asked for so the open panel shows it without waiting for a key.
pub(crate) fn begin_checks(
    tx: &crate::app_event_sender::AppEventSender,
    frames: crate::tui::FrameRequester,
) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    for agent in nori_harness::list_available_agents() {
        let Some(account) = nori_acp_host::accounts::find(&agent.agent_name) else {
            continue;
        };
        if !agent.is_installed || !nori_acp_host::accounts::status::can_check(&account) {
            continue;
        }
        nori_acp_host::accounts::status::mark_checking(&agent.agent_name);
        let (tx, slug, frames) = (tx.clone(), agent.agent_name, frames.clone());
        runtime.spawn(async move {
            nori_acp_host::accounts::status::check(&slug).await;
            tx.send(AppEvent::AccountChecked { slug });
            frames.schedule_frame_in(std::time::Duration::from_millis(30));
        });
    }
}

/// How `agent` stands, from its own check when it has one.
fn standing_of(
    agent: &AcpAgentInfo,
    account: Option<&Account>,
    identity: Option<&AccountIdentity>,
    nori_home: &Path,
    active_agent: &str,
) -> (Standing, Option<String>) {
    if !agent.is_installed {
        return (Standing::NotInstalled, None);
    }
    if account.is_some_and(nori_acp_host::accounts::status::can_check) {
        // A check that is running again outranks the last verdict: what was true a minute ago
        // (signed in, before the user signed out elsewhere) is not said as if it were now.
        return checked_standing(
            nori_acp_host::accounts::status::is_checking(&agent.agent_name),
            nori_acp_host::accounts::status::latest(&agent.agent_name),
        );
    }
    // A provider with no check: what its agent said, and whether it lists models. An agent that
    // says it is signed out is, whatever it listed before; one that says who it is signed in as
    // is signed in; the agent in use is signed in whether or not it lists models.
    let standing = if identity.is_some_and(|who| who.kind == "none") {
        Standing::SignIn
    } else if identity.is_some()
        || agent.agent_name == active_agent
        || !nori_harness::cached_agent_models(nori_home, &agent.agent_name).is_empty()
    {
        Standing::SignedIn
    } else {
        Standing::SignIn
    };
    (standing, None)
}

/// The current check takes precedence over its most recent verdict.
fn checked_standing(checking: bool, latest: Option<AccountStatus>) -> (Standing, Option<String>) {
    if checking {
        return (Standing::Checking, None);
    }
    match latest {
        Some(AccountStatus::SignedIn(_)) => (Standing::SignedIn, None),
        Some(AccountStatus::NotSignedIn(reason)) => (Standing::SignIn, Some(reason)),
        Some(AccountStatus::Unknown(reason)) => (Standing::Unknown, Some(reason)),
        None => (Standing::Unknown, Some("not checked yet".to_owned())),
    }
}

fn row_of(
    agent: AcpAgentInfo,
    nori_home: &Path,
    active_agent: &str,
    headroom: &AccountHeadroom,
) -> Row {
    let identity = nori_acp_host::accounts::identity_of(&agent.agent_name);
    let account = nori_acp_host::accounts::find(&agent.agent_name);
    let (standing, reason) = standing_of(
        &agent,
        account.as_ref(),
        identity.as_ref(),
        nori_home,
        active_agent,
    );
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
    let not_installed = standing == Standing::NotInstalled;
    let install = not_installed
        .then(|| nori_acp_host::accounts::install_command(agent.agent))
        .flatten();
    let instead = not_installed.then(|| instead_of(&agent)).flatten();
    Row {
        standing,
        reason,
        identity,
        account,
        agent,
        room,
        install,
        instead,
    }
}

/// The providers panel for a Nori home, with `active_agent` marked as the one in use.
pub(crate) fn spec(nori_home: &Path, active_agent: &str) -> ChoiceSpec {
    let headroom = AccountHeadroom::load(nori_home);
    let rows = nori_harness::list_available_agents()
        .into_iter()
        // Debug fixtures are not providers, unless a session is running on one.
        .filter(|agent| {
            !nori_harness::recovery::is_debug_fixture(&agent.agent_name)
                || nori_harness::recovery::is_debug_fixture(active_agent)
        })
        .map(|agent| row_of(agent, nori_home, active_agent, &headroom))
        .collect();
    spec_of_rows(nori_home, active_agent, rows)
}

fn spec_of_rows(nori_home: &Path, active_agent: &str, rows: Vec<Row>) -> ChoiceSpec {
    // Accounts by provider, in the order the registry lists them; `None` holds the agents that
    // belong to no provider (a custom agent, a debug fixture in use).
    let mut groups: Vec<(Option<&'static ProviderAccounts>, Vec<Row>)> = Vec::new();
    for row in rows {
        let provider = row.account.as_ref().map(|account| account.provider);
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

    let place = Place {
        home: Arc::new(nori_home.to_path_buf()),
        active: Arc::from(active_agent),
    };
    let back: StepAction = {
        let place = place.clone();
        Arc::new(move |_| Next::Show(spec(&place.home, &place.active)))
    };
    let mut items = Vec::new();
    for (provider, rows) in groups {
        let section = provider.map_or("Other agents", |provider| provider.title);
        for row in rows {
            let current = row.agent.agent_name == active_agent;
            let detail = summary(&row);
            let keywords = row.agent.agent_name.clone();
            let label = row.agent.display_name.clone();
            let (back, place) = (back.clone(), place.clone());
            let mut item = ChoiceItem::step(
                label,
                Arc::new(move |_| Next::Show(account_spec(&row, back.clone(), &place))),
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
    items.push(super::fallbacks_panel::entry(place.home.clone(), back).section("Routing"));
    let mut spec = ChoiceSpec::new("Providers", items);
    spec.subtitle = Some("Enter opens an account: sign in, see its login, add another".to_owned());
    // A check landed: the rows are drawn again from what it found.
    spec.on_notice = Some(Arc::new(move |notice, _| {
        notice
            .downcast_ref::<AccountChecked>()
            .map(|_| Next::Show(self::spec(&place.home, &place.active)))
    }));
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
        Standing::Checking => match who.filter(|_| {
            row.identity
                .as_ref()
                .is_some_and(|identity| identity.kind != "none")
        }) {
            Some(who) => format!("checking… · last seen as {who}"),
            None => "checking…".to_owned(),
        },
        Standing::SignIn => match &row.reason {
            Some(reason) => format!("not signed in · {reason} · Enter to sign in"),
            None => "not signed in · Enter to sign in".to_owned(),
        },
        Standing::Unknown => match &row.reason {
            Some(reason) => format!("can't tell if it is signed in · {reason}"),
            None => "can't tell if it is signed in".to_owned(),
        },
        Standing::NotInstalled => match &row.install {
            Some(command) => format!("not installed · {command}"),
            None => format!("not installed · {}", row.agent.description),
        },
    }
}

/// One account's page: signing in, and where it lives.
fn account_spec(row: &Row, back: StepAction, place: &Place) -> ChoiceSpec {
    let agent_name = row.agent.agent_name.clone();
    let (label, detail) = match row.standing {
        Standing::SignedIn => ("Sign in again", "renew this login, or use another way"),
        Standing::Checking | Standing::SignIn | Standing::Unknown => {
            ("Sign in", "the ways this account can sign in")
        }
        Standing::NotInstalled => ("How to install", "what to run, and what else works"),
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
        && row.standing == Standing::SignedIn
    {
        items.push(info("Signed in as", who.describe()));
    }
    if let Some(reason) = &row.reason {
        items.push(info("Status", reason.clone()));
    }
    if row.standing == Standing::NotInstalled
        && let Some(command) = &row.install
    {
        items.push(info("Install with", command.clone()));
        if let Some(instead) = &row.instead {
            items.push(info("Personal Google account", instead.clone()));
        }
        items.push(info(
            "API key or Vertex AI",
            format!(
                "once the CLI is installed, set {} (or the Vertex AI variables) where Nori starts",
                nori_acp_host::accounts::gemini_cli::KEY_VARIABLE
            ),
        ));
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
    // This account's check landed: the page is drawn again from what it found.
    let (agent, slug) = (row.agent.clone(), row.agent.agent_name.clone());
    let (place, back_again) = (place.clone(), back.clone());
    page.on_notice = Some(Arc::new(move |notice, _| {
        let checked = notice.downcast_ref::<AccountChecked>()?;
        if checked.0 != slug {
            return None;
        }
        let headroom = AccountHeadroom::load(&place.home);
        let row = row_of(agent.clone(), &place.home, &place.active, &headroom);
        Some(Next::Show(account_spec(&row, back_again.clone(), &place)))
    }));
    page.on_back = Some(back);
    page
}

/// What else signs in when `agent` is not installed. Only the Gemini CLI has an answer: personal
/// Google accounts moved to Antigravity, whose own state is said as it stands.
fn instead_of(agent: &AcpAgentInfo) -> Option<String> {
    if agent.agent != AgentKind::Gemini {
        return None;
    }
    let antigravity = nori_harness::list_available_agents()
        .into_iter()
        .find(|other| other.agent == AgentKind::Antigravity);
    Some(match antigravity {
        Some(installed) if installed.is_installed => {
            match nori_acp_host::accounts::status::latest(&installed.agent_name) {
                Some(AccountStatus::SignedIn(_)) => {
                    "Antigravity is installed and signed in: Google's Gemini models run there"
                }
                Some(AccountStatus::NotSignedIn(_)) => {
                    "Antigravity is installed but not signed in: sign in there for Google's Gemini models"
                }
                Some(AccountStatus::Unknown(_)) | None => {
                    "Antigravity is installed: sign in there for Google's Gemini models"
                }
            }
        }
        Some(_) | None => "Antigravity (not installed either) is where personal Google accounts go",
    }
    .to_owned())
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
                secret: false,
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

#[cfg(test)]
mod tests {
    use nori_config::PROVIDERS;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::app_event_sender::AppEventSender;
    use crate::choice_picker::ChoiceOverlay;

    fn row(
        slug: &str,
        name: &str,
        standing: Standing,
        reason: Option<&str>,
        email: Option<&str>,
    ) -> Row {
        let origin = if slug == "claude-code" {
            Origin::Default
        } else {
            Origin::Native
        };
        Row {
            standing,
            reason: reason.map(str::to_owned),
            agent: AcpAgentInfo {
                agent: AgentKind::ClaudeCode,
                agent_name: slug.to_owned(),
                display_name: name.to_owned(),
                description: "Claude Code".to_owned(),
                provider_slug: slug.to_owned(),
                is_installed: true,
                managed_by: None,
            },
            account: Some(Account {
                slug: slug.to_owned(),
                provider: &PROVIDERS[0],
                name: slug.to_owned(),
                display_name: name.to_owned(),
                origin,
                home: Home::At(PathBuf::from(format!("/nori/accounts/claude/{slug}"))),
            }),
            identity: email.map(|email| AccountIdentity {
                kind: "account".to_owned(),
                label: "Claude max".to_owned(),
                email: Some(email.to_owned()),
                organization: None,
                plan: Some("max".to_owned()),
            }),
            room: None,
            install: None,
            instead: None,
        }
    }

    /// A Google account: `slug` is `antigravity` or `gemini`, the CLI's own login, no identity.
    fn google_row(slug: &str, name: &str, standing: Standing, reason: Option<&str>) -> Row {
        let (agent, provider) = match slug {
            "gemini" => (AgentKind::Gemini, "gemini"),
            _ => (AgentKind::Antigravity, "antigravity"),
        };
        Row {
            standing,
            reason: reason.map(str::to_owned),
            agent: AcpAgentInfo {
                agent,
                agent_name: slug.to_owned(),
                display_name: name.to_owned(),
                description: "Google".to_owned(),
                provider_slug: slug.to_owned(),
                is_installed: standing != Standing::NotInstalled,
                managed_by: None,
            },
            account: Some(Account {
                slug: slug.to_owned(),
                provider: ProviderAccounts::find(provider).expect("a known provider"),
                name: "default".to_owned(),
                display_name: name.to_owned(),
                origin: Origin::Default,
                home: Home::Default,
            }),
            identity: None,
            room: None,
            install: None,
            instead: None,
        }
    }

    fn overlay(spec: ChoiceSpec) -> ChoiceOverlay {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        ChoiceOverlay::new(spec, AppEventSender::new(tx))
    }

    fn panel(rows: Vec<Row>) -> ChoiceSpec {
        spec_of_rows(Path::new("/nori"), "claude-code", rows)
    }

    #[test]
    fn the_panel_says_each_accounts_verified_state() {
        let spec = panel(vec![
            row(
                "claude-code",
                "Claude Code",
                Standing::SignedIn,
                None,
                Some("robert@example.org"),
            ),
            row(
                "claude@f",
                "Claude · f",
                Standing::SignedIn,
                None,
                Some("f@example.org"),
            ),
            row(
                "claude@g",
                "Claude · g",
                Standing::SignIn,
                Some("no login found in this account's home"),
                None,
            ),
            row("claude@h", "Claude · h", Standing::Checking, None, None),
            row(
                "claude@i",
                "Claude · i",
                Standing::Unknown,
                Some("`claude` is not installed"),
                None,
            ),
        ]);
        insta::assert_snapshot!(overlay(spec).render_text(100, 16));
    }

    #[test]
    fn an_account_added_but_never_signed_in_is_not_signed_in() {
        // What the check finds in an account's home that no login ever reached.
        let abandoned = row(
            "claude@f",
            "Claude · f",
            Standing::SignIn,
            Some("no login found in this account's home"),
            None,
        );
        let summary = summary(&abandoned);
        assert_eq!(
            summary,
            "not signed in · no login found in this account's home · Enter to sign in"
        );
        let page = account_spec(
            &abandoned,
            Arc::new(|_| Next::Done),
            &Place {
                home: Arc::new(PathBuf::from("/nori")),
                active: Arc::from("claude-code"),
            },
        );
        insta::assert_snapshot!(overlay(page).render_text(100, 12));
    }

    #[test]
    fn a_verified_row_shows_the_email_and_a_checking_row_never_claims_to_be_signed_in() {
        let verified = row(
            "claude@f",
            "Claude · f",
            Standing::SignedIn,
            None,
            Some("f@example.org"),
        );
        assert_eq!(summary(&verified), "f@example.org · Max");
        let checking = row(
            "claude@f",
            "Claude · f",
            Standing::Checking,
            None,
            Some("f@example.org"),
        );
        assert_eq!(
            summary(&checking),
            "checking… · last seen as f@example.org · Max"
        );
    }

    #[test]
    fn the_google_rows_say_what_their_session_check_found() {
        let spec = spec_of_rows(
            Path::new("/nori"),
            "antigravity",
            vec![
                google_row(
                    "antigravity",
                    "Antigravity",
                    Standing::SignIn,
                    Some("Authentication required"),
                ),
                google_row(
                    "gemini",
                    "Gemini",
                    Standing::SignIn,
                    Some("Gemini API key is missing or not configured."),
                ),
            ],
        );
        insta::assert_snapshot!(overlay(spec).render_text(110, 12));

        let unproven = google_row(
            "antigravity",
            "Antigravity",
            Standing::Unknown,
            Some(
                "Antigravity opened a session but listed no models, so Nori cannot tell its login works",
            ),
        );
        assert_eq!(
            summary(&unproven),
            "can't tell if it is signed in · Antigravity opened a session but listed no models, so Nori cannot tell its login works"
        );
    }

    #[test]
    fn a_gemini_that_is_not_installed_says_the_command_and_what_else_works() {
        let mut gemini = google_row("gemini", "Gemini", Standing::NotInstalled, None);
        gemini.install = Some("brew install gemini-cli".to_owned());
        gemini.instead = Some(
            "Antigravity is installed and signed in: Google's Gemini models run there".to_owned(),
        );
        assert_eq!(summary(&gemini), "not installed · brew install gemini-cli");
        let page = account_spec(
            &gemini,
            Arc::new(|_| Next::Done),
            &Place {
                home: Arc::new(PathBuf::from("/nori")),
                active: Arc::from("antigravity"),
            },
        );
        insta::assert_snapshot!(overlay(page).render_text(120, 14));
    }

    #[test]
    fn a_check_running_again_outranks_the_verdict_of_the_last_one() {
        let (standing, reason) = checked_standing(
            true,
            Some(AccountStatus::SignedIn(AccountIdentity {
                kind: "account".to_owned(),
                label: "Google account".to_owned(),
                email: None,
                organization: None,
                plan: None,
            })),
        );
        assert_eq!((standing, reason), (Standing::Checking, None));
    }

    #[test]
    fn a_landed_check_redraws_the_panel_and_leaves_other_notices_alone() {
        let spec = panel(Vec::new());
        let notify = spec.on_notice.expect("the panel listens for checks");
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let tx = AppEventSender::new(tx);
        assert!(notify(&AccountChecked("claude@f".to_owned()), &tx).is_some());
        assert!(notify(&"something else", &tx).is_none());
    }
}
