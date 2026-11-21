//! Whether an account is signed in, found out by asking its own login, never assumed.
//!
//! An account is only "signed in" when its provider's own check says so, run against that
//! account's home:
//!
//! - **Claude**: `claude auth status --json`, the real executable with the account's
//!   `CLAUDE_CONFIG_DIR` (so a launcher that pins another profile cannot answer for it);
//! - **Codex** and **OpenCode**: the in-process agent's auth probe, which reads the stored
//!   sign-in of that home (Codex also notices an expired ChatGPT login its refresh token could
//!   not renew). Nothing is sent to a model, so a check spends no usage.
//!
//! - **Gemini** and **Antigravity** keep their login in the CLI's own store (a file, the OS
//!   keyring) that Nori never opens, and have no status command, so the check is what the CLI
//!   itself does: start its ACP agent and open a session. Both refuse `session/new` without a
//!   login, with the ACP authentication-required error (`-32000`: Antigravity says
//!   "Authentication required", Gemini "Gemini API key is missing or not configured."), which is
//!   "not signed in". A session that opens is not yet proof for Antigravity, whose sign-in is
//!   found silently (its keyring): it must also list models (its `model` config option, which the
//!   CLI leaves empty without a sign-in mode), else the answer is "can't tell". No prompt is sent,
//!   so no usage is spent.
//!
//! The answer has three honest forms ([`AccountStatus`]): signed in (as whom, when known), not
//! signed in (and why), and unknown (the check could not run or the provider has none). A check
//! runs on a task; until the first answer lands the account is "checking"
//! ([`is_checking`]). What is learned is filed in [`identity_of`](super::identity_of) so the next
//! launch can show who the account was, but a remembered identity is never a verdict.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Mutex;
use std::time::Duration;

use nori_acp_kit::AuthStatus;
use nori_config::AcpProxyConfig;
use nori_protocol::acp::v1 as acp;
use serde_json::Value;

use super::AccountIdentity;
use super::launch::claude_status_program;
use super::record_identity;
use crate::connection::acp_connection::AcpConnection;
use crate::error_category::FailureClass;
use crate::error_category::classify_failure;
use crate::registry::AcpAgentConfig;
use crate::registry::AgentKind;

/// How long a check may take. A hung CLI must not leave a row "checking" forever.
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(12);
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// Starting an ACP agent (a Node CLI, a worker process) takes longer than reading a login.
const SESSION_TIMEOUT: Duration = Duration::from_secs(40);

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    /// The account's own login is there, and says who it is.
    SignedIn(AccountIdentity),
    /// It is not, and in words why (and so what to do).
    NotSignedIn(String),
    /// The check could not run, or the provider has none.
    Unknown(String),
}

impl AccountStatus {
    pub fn is_signed_in(&self) -> bool {
        matches!(self, Self::SignedIn(_))
    }

    /// The status an agent or probe reported, as a verdict.
    fn of(status: &AuthStatus) -> Self {
        match status.kind.as_str() {
            "none" => Self::NotSignedIn(
                status
                    .detail
                    .clone()
                    .unwrap_or_else(|| "no login found in this account's home".to_owned()),
            ),
            "unknown" => Self::Unknown(
                status
                    .detail
                    .clone()
                    .unwrap_or_else(|| "it could not be checked".to_owned()),
            ),
            _ => Self::SignedIn(AccountIdentity::from_status(status)),
        }
    }
}

#[derive(Default)]
struct Book {
    /// The newest answer for each account, this run of Nori.
    results: HashMap<String, AccountStatus>,
    /// Accounts with a check in flight.
    checking: HashSet<String>,
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);

fn with_book<T>(use_book: impl FnOnce(&mut Book) -> T) -> T {
    let mut guard = BOOK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    use_book(guard.get_or_insert_with(Book::default))
}

/// The newest answer for `slug` found since Nori started, if any.
pub fn latest(slug: &str) -> Option<AccountStatus> {
    with_book(|book| book.results.get(slug).cloned())
}

/// Whether a check of `slug` is running.
pub fn is_checking(slug: &str) -> bool {
    with_book(|book| book.checking.contains(slug))
}

/// Say a check of `slug` is about to run, so a panel drawn before it starts already says
/// "checking". [`check`] ends it.
pub fn mark_checking(slug: &str) {
    with_book(|book| book.checking.insert(slug.to_owned()));
}

/// Whether Nori can verify an account's login at all: every provider it signs in to has a check.
pub fn can_check(account: &super::Account) -> bool {
    matches!(
        account.provider.id,
        "claude" | "codex" | "opencode" | "gemini" | "antigravity"
    )
}

/// Ask the account's own login whether it is signed in, and file the answer. Runs the provider's
/// check; the caller runs it on a task.
pub async fn check(slug: &str) -> AccountStatus {
    mark_checking(slug);
    let reported = ask(slug).await;
    let status = AccountStatus::of(&reported);
    record_identity(slug, &reported);
    with_book(|book| {
        book.results.insert(slug.to_owned(), status.clone());
        book.checking.remove(slug);
    });
    status
}

async fn ask(slug: &str) -> AuthStatus {
    let Some(account) = super::find(slug) else {
        return AuthStatus::unknown(format!("{slug} is not an account Nori knows"));
    };
    match account.provider.id {
        "claude" => match claude_status_program(&account) {
            Ok((program, env)) => ask_claude(&program, &env).await,
            Err(reason) => AuthStatus::unknown(reason),
        },
        "codex" | "opencode" => {
            let Ok(config) = crate::registry::get_agent_config(slug) else {
                return AuthStatus::unknown(format!("{slug} has no agent to ask"));
            };
            match tokio::time::timeout(PROBE_TIMEOUT, crate::in_process::probe_auth(&config)).await
            {
                Ok(Some(status)) => status,
                Ok(None) => AuthStatus::unknown(format!(
                    "{} runs outside Nori, so its login is checked when it opens a session",
                    account.display_name
                )),
                Err(_) => AuthStatus::unknown("the check took too long"),
            }
        }
        "gemini" | "antigravity" => ask_by_session(slug).await,
        _ => AuthStatus::unknown(format!(
            "Nori has no way to check {}'s login",
            account.provider.title
        )),
    }
}

/// Start the agent and open a session: a CLI that is not signed in refuses with its
/// authentication-required error.
async fn ask_by_session(slug: &str) -> AuthStatus {
    let config = match crate::registry::get_agent_config(slug) {
        Ok(config) => config,
        // Why there is nothing to start (not installed, too old to speak ACP) is the answer.
        Err(error) => return AuthStatus::unknown(format!("{error:#}")),
    };
    // A signed-in `agy acp` offers its models; one that lists none has not shown a login works.
    let needs_models = config.agent == AgentKind::Antigravity;
    session_status(&config, needs_models).await
}

/// What opening a session on `config`'s agent showed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SessionOpened {
    /// It offered at least one model to choose from.
    lists_models: bool,
}

/// Whether `options` offer at least one model, the way `/model` reads them.
fn offers_a_model(options: &[acp::SessionConfigOption]) -> bool {
    options
        .iter()
        .filter(|option| {
            option.category == Some(acp::SessionConfigOptionCategory::Model)
                || option.id.to_string() == "model"
        })
        .any(|option| match &option.kind {
            acp::SessionConfigKind::Select(select) => match &select.options {
                acp::SessionConfigSelectOptions::Ungrouped(options) => !options.is_empty(),
                acp::SessionConfigSelectOptions::Grouped(groups) => {
                    groups.iter().any(|group| !group.options.is_empty())
                }
                // A layout this schema does not know offers nothing Nori can read.
                _ => false,
            },
            _ => false,
        })
}

/// Start `config`'s agent, open a session and say what that proves about its login. With
/// `needs_models`, opening one is not enough: it must offer models.
async fn session_status(config: &AcpAgentConfig, needs_models: bool) -> AuthStatus {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir());
    let attempt = async {
        let connection = AcpConnection::spawn(config, &cwd, AcpProxyConfig::disabled()).await?;
        let session = connection.create_session(&cwd, Vec::new()).await;
        let opened = session.map(|_| SessionOpened {
            lists_models: offers_a_model(&connection.config_options()),
        });
        connection.shutdown().await;
        opened
    };
    match tokio::time::timeout(SESSION_TIMEOUT, attempt).await {
        Ok(outcome) => session_verdict(&config.display_name, needs_models, outcome),
        Err(_) => AuthStatus::unknown("the agent took too long to open a session"),
    }
}

/// What `outcome` of opening a session says: a login (only with the proof `needs_models` asks
/// for), no login (the agent's own refusal, in its words), or nothing certain.
fn session_verdict(
    display_name: &str,
    needs_models: bool,
    outcome: anyhow::Result<SessionOpened>,
) -> AuthStatus {
    match outcome {
        Ok(opened) if needs_models && !opened.lists_models => AuthStatus::unknown(format!(
            "{display_name} opened a session but listed no models, so Nori cannot tell its login works"
        )),
        Ok(_) => AuthStatus {
            kind: "external".to_owned(),
            label: "signed in".to_owned(),
            detail: None,
            account: None,
        },
        Err(error) => {
            let failure = classify_failure(&error);
            match failure.class {
                FailureClass::Auth => {
                    let mut status = AuthStatus::signed_out();
                    status.detail = failure
                        .detail
                        .or(failure.message)
                        .or_else(|| Some("it says authentication is required".to_owned()));
                    status
                }
                FailureClass::ExecutableNotFound => {
                    AuthStatus::unknown(format!("{display_name} is not installed"))
                }
                _ => AuthStatus::unknown(format!("{error:#}")),
            }
        }
    }
}

/// Run `program auth status --json` with `env`, and read the answer. A logged-out Claude exits
/// non-zero but still answers, so the answer decides, not the exit code.
pub(super) async fn ask_claude(program: &str, env: &HashMap<String, String>) -> AuthStatus {
    let mut command = tokio::process::Command::new(program);
    command
        .args(["auth", "status", "--json"])
        .envs(env)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(CLAUDE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return AuthStatus::unknown(format!("`{program}` is not installed"));
        }
        Ok(Err(error)) => {
            return AuthStatus::unknown(format!("could not run `{program}`: {error}"));
        }
        Err(_) => return AuthStatus::unknown("`claude auth status` took too long"),
    };
    std::str::from_utf8(&output.stdout)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .and_then(|json| AuthStatus::from_claude_cli(&json))
        .unwrap_or_else(|| {
            AuthStatus::unknown("`claude auth status` gave an answer Nori cannot read")
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use pretty_assertions::assert_eq;

    use super::*;

    /// A stand-in `claude` that prints `json` for `auth status --json` and exits `code`.
    fn fake_claude(dir: &Path, json: &str, code: i32) -> String {
        let path = dir.join("claude");
        std::fs::write(
            &path,
            format!("#!/bin/sh\ncat <<'JSON'\n{json}\nJSON\nexit {code}\n"),
        )
        .expect("write the fake claude");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn a_signed_in_claude_names_the_email() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = fake_claude(
            dir.path(),
            r#"{"loggedIn":true,"authMethod":"claude.ai","email":"f@example.org","orgName":"F","subscriptionType":"max"}"#,
            0,
        );
        let status = AccountStatus::of(&ask_claude(&program, &HashMap::new()).await);
        assert_eq!(
            status,
            AccountStatus::SignedIn(AccountIdentity {
                kind: "account".to_owned(),
                label: "Claude max".to_owned(),
                email: Some("f@example.org".to_owned()),
                organization: Some("F".to_owned()),
                plan: Some("max".to_owned()),
            })
        );
    }

    #[tokio::test]
    async fn a_logged_out_claude_is_not_signed_in_despite_the_json_being_well_formed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = fake_claude(dir.path(), r#"{"loggedIn":false,"authMethod":"none"}"#, 1);
        let status = AccountStatus::of(&ask_claude(&program, &HashMap::new()).await);
        assert_eq!(
            status,
            AccountStatus::NotSignedIn("no login found in this account's home".to_owned())
        );
    }

    #[tokio::test]
    async fn a_claude_that_answers_nonsense_is_unknown_not_signed_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let program = fake_claude(dir.path(), "welcome to claude", 0);
        let status = AccountStatus::of(&ask_claude(&program, &HashMap::new()).await);
        assert_eq!(
            status,
            AccountStatus::Unknown(
                "`claude auth status` gave an answer Nori cannot read".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn a_missing_claude_is_unknown() {
        let status = AccountStatus::of(
            &ask_claude("/nonexistent/claude-for-nori-tests", &HashMap::new()).await,
        );
        assert_eq!(
            status,
            AccountStatus::Unknown(
                "`/nonexistent/claude-for-nori-tests` is not installed".to_owned()
            )
        );
    }

    #[tokio::test]
    async fn the_account_home_reaches_the_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Signed in only when asked about the home named f.
        let path = dir.path().join("claude");
        std::fs::write(
            &path,
            "#!/bin/sh\nif [ \"$CLAUDE_CONFIG_DIR\" = f ]; then echo '{\"loggedIn\":true,\"email\":\"f@example.org\",\"subscriptionType\":\"pro\"}'; else echo '{\"loggedIn\":false}'; fi\n",
        )
        .expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let program = path.to_string_lossy().into_owned();
        let signed_in = AccountStatus::of(
            &ask_claude(
                &program,
                &HashMap::from([("CLAUDE_CONFIG_DIR".to_owned(), "f".to_owned())]),
            )
            .await,
        );
        let other = AccountStatus::of(
            &ask_claude(
                &program,
                &HashMap::from([("CLAUDE_CONFIG_DIR".to_owned(), "g".to_owned())]),
            )
            .await,
        );
        assert_eq!(
            (signed_in.is_signed_in(), other.is_signed_in()),
            (true, false)
        );
    }

    fn refused(error: acp::Error) -> anyhow::Error {
        anyhow::Error::new(error).context("Failed to create ACP session")
    }

    fn model_option() -> acp::SessionConfigOption {
        acp::SessionConfigOption::select(
            "model",
            "Model",
            "gemini-3.8-flash",
            vec![acp::SessionConfigSelectOption::new(
                "gemini-3.8-flash",
                "Gemini 3.8 Flash",
            )],
        )
        .category(acp::SessionConfigOptionCategory::Model)
    }

    fn mode_option() -> acp::SessionConfigOption {
        acp::SessionConfigOption::select(
            "mode",
            "Mode",
            "default",
            vec![acp::SessionConfigSelectOption::new("default", "Default")],
        )
        .category(acp::SessionConfigOptionCategory::Mode)
    }

    #[test]
    fn antigravity_refusing_the_session_for_want_of_a_login_is_not_signed_in() {
        // What `agy acp` answers `session/new` with when its silent sign-in found nothing,
        // read from a real agy 1.5.11 run against an empty home.
        let status = AccountStatus::of(&session_verdict(
            "Antigravity",
            true,
            Err(refused(acp::Error::new(-32000, "Authentication required"))),
        ));
        assert_eq!(
            status,
            AccountStatus::NotSignedIn("Authentication required".to_owned())
        );
    }

    #[test]
    fn geminis_refusal_is_not_signed_in_in_its_own_words() {
        // What `gemini --acp` 0.47.0 answers `session/new` with when no key or login is set.
        let status = AccountStatus::of(&session_verdict(
            "Gemini",
            false,
            Err(refused(acp::Error::new(
                -32000,
                "Gemini API key is missing or not configured.",
            ))),
        ));
        assert_eq!(
            status,
            AccountStatus::NotSignedIn("Gemini API key is missing or not configured.".to_owned())
        );
    }

    #[test]
    fn antigravity_opening_a_session_with_no_models_cannot_be_told_to_work() {
        let status = AccountStatus::of(&session_verdict(
            "Antigravity",
            true,
            Ok(SessionOpened {
                lists_models: false,
            }),
        ));
        assert_eq!(
            status,
            AccountStatus::Unknown(
                "Antigravity opened a session but listed no models, so Nori cannot tell its login works"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_session_that_lists_models_is_a_login_that_works() {
        let opened = SessionOpened { lists_models: true };
        assert!(
            AccountStatus::of(&session_verdict("Antigravity", true, Ok(opened))).is_signed_in()
        );
        // Gemini is not asked for models: its session opens only once its auth took.
        let bare = SessionOpened {
            lists_models: false,
        };
        assert!(AccountStatus::of(&session_verdict("Gemini", false, Ok(bare))).is_signed_in());
    }

    #[test]
    fn a_failure_that_is_not_a_refusal_is_unknown_never_signed_in_or_out() {
        let status = AccountStatus::of(&session_verdict(
            "Antigravity",
            true,
            Err(refused(acp::Error::internal_error())),
        ));
        assert!(
            matches!(status, AccountStatus::Unknown(_)),
            "an agent that errored for another reason says nothing about the login: {status:?}"
        );
        let status = AccountStatus::of(&session_verdict(
            "Antigravity",
            true,
            Err(anyhow::anyhow!(
                "agent exited before initialization finished"
            )),
        ));
        assert_eq!(
            status,
            AccountStatus::Unknown("agent exited before initialization finished".to_owned())
        );
    }

    #[test]
    fn only_a_model_option_with_a_model_in_it_counts_as_models() {
        assert!(offers_a_model(&[mode_option(), model_option()]));
        assert!(!offers_a_model(&[mode_option()]));
        assert!(!offers_a_model(&[]));
        let empty = acp::SessionConfigOption::select(
            "model",
            "Model",
            "none",
            Vec::<acp::SessionConfigSelectOption>::new(),
        )
        .category(acp::SessionConfigOptionCategory::Model);
        assert!(!offers_a_model(&[empty]));
    }

    /// A stand-in ACP agent: answers `initialize`, and `session/new` with `reply`.
    fn fake_agent(dir: &Path, reply: &str) -> AcpAgentConfig {
        let path = dir.join("fake-agent");
        let script = format!(
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\("[^"]*"\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{{"jsonrpc":"2.0","id":%s,"result":{{"protocolVersion":1,"agentCapabilities":{{}},"authMethods":[]}}}}\n' "$id" ;;
    *'"method":"session/new"'*)
      printf '%s\n' '{reply}' | sed "s/__ID__/$id/" ;;
  esac
done
"#
        );
        std::fs::write(&path, script).expect("write the fake agent");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("make it executable");
        AcpAgentConfig {
            agent: AgentKind::Antigravity,
            provider_slug: "antigravity".to_owned(),
            command: path.to_string_lossy().into_owned(),
            args: Vec::new(),
            env: HashMap::new(),
            provider_info: crate::registry::AcpProviderInfo::default(),
            auth_hint: String::new(),
            display_name: "Antigravity".to_owned(),
            install_hint: String::new(),
            model_injection: crate::registry::ModelInjection::None,
            advertise_subagents: false,
            in_process: None,
        }
    }

    const REFUSES: &str = r#"{"jsonrpc":"2.0","id":__ID__,"error":{"code":-32000,"message":"Authentication required"}}"#;
    const OPENS_WITH_MODELS: &str = r#"{"jsonrpc":"2.0","id":__ID__,"result":{"sessionId":"s1","configOptions":[{"id":"model","name":"Model","category":"model","type":"select","currentValue":"a","options":[{"value":"a","name":"Gemini 3.8 Flash"}]}]}}"#;
    const OPENS_BARE: &str = r#"{"jsonrpc":"2.0","id":__ID__,"result":{"sessionId":"s1"}}"#;

    #[tokio::test]
    async fn a_started_agent_that_refuses_the_session_reads_not_signed_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fake_agent(dir.path(), REFUSES);
        let status = AccountStatus::of(&session_status(&config, true).await);
        assert_eq!(
            status,
            AccountStatus::NotSignedIn("Authentication required".to_owned())
        );
    }

    #[tokio::test]
    async fn a_started_agent_that_opens_a_session_with_models_reads_signed_in() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fake_agent(dir.path(), OPENS_WITH_MODELS);
        assert!(AccountStatus::of(&session_status(&config, true).await).is_signed_in());
    }

    #[tokio::test]
    async fn a_started_agent_that_opens_a_session_without_models_is_not_trusted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = fake_agent(dir.path(), OPENS_BARE);
        let status = AccountStatus::of(&session_status(&config, true).await);
        assert!(
            matches!(status, AccountStatus::Unknown(_)),
            "a session with no models proves nothing: {status:?}"
        );
        // The same agent, where models are not the proof, opened its session.
        assert!(AccountStatus::of(&session_status(&config, false).await).is_signed_in());
    }

    #[tokio::test]
    async fn a_check_ends_its_checking_and_leaves_its_answer() {
        let slug = "claude@nobody";
        mark_checking(slug);
        assert!(is_checking(slug));
        let status = check(slug).await;
        assert_eq!(
            status,
            AccountStatus::Unknown("claude@nobody is not an account Nori knows".to_owned())
        );
        assert!(!is_checking(slug));
        assert_eq!(latest(slug), Some(status));
    }
}
