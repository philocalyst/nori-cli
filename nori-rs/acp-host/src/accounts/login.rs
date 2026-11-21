//! Every way to sign in to an account, as Nori offers them.
//!
//! Each provider's own tool says what it supports; this is that list, run against the account's
//! home so the login lands where that account's agent reads it:
//!
//! | Provider | Ways |
//! | --- | --- |
//! | Claude | `claude auth login` for a Claude subscription (browser, or the code pasted back), the same with an email pre-filled, with SSO, or for the Anthropic Console; a long-lived token (`claude setup-token`, or one pasted); an API key |
//! | Codex | ChatGPT in the browser (the agent's own `authenticate`), a device code (`codex login --device-auth`), an API key |
//! | OpenCode | an OpenCode API key, which Nori keeps for the account |
//! | Antigravity | `agy acp --login`: Google sign-in in the browser, or over SSH the code it shows, pasted |
//! | Gemini | what the Gemini CLI offers over ACP (`authenticate`: Login with Google, an API key); it has no `login` command |
//!
//! A way that needs an answer (an email, a key) carries the question ([`SignInAsk`]); the caller
//! asks it and gets the final method from [`SignInMethod::answered`]. A key or token is never
//! stored by Nori except where the provider keeps it: Claude's `settings.json` `env` of the
//! account's own home (the documented place for `ANTHROPIC_API_KEY` and
//! `CLAUDE_CODE_OAUTH_TOKEN`), Codex's own `auth.json` through its `authenticate`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use anyhow::Result;

use super::Account;
use super::Home;
use super::Origin;
use super::launch::LoginProgram;
use super::launch::default_environment;
use super::launch::runs_executable;
use super::unwrap::unwrap_executable;

/// A secret the user typed: held to be used, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The secret itself, for the one place that hands it on.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(<hidden>)")
    }
}

/// A token a sign-in program prints when it is done, for Nori to keep where the provider reads
/// it (`claude setup-token` prints one and saves nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capture {
    /// How the token starts.
    pub prefix: &'static str,
    /// The `env` entry of the account's Claude `settings.json` it is kept under.
    pub variable: &'static str,
}

/// Where an answer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// After this flag in the sign-in program's arguments (`--email`).
    Arg(&'static str),
    /// In this environment variable of the agent, for this one sign-in only.
    AgentEnv(&'static str),
    /// In this `env` entry of the account's Claude `settings.json`.
    ClaudeSetting(&'static str),
}

/// What a way to sign in needs to be told first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInAsk {
    /// The question, as a label: `email`, `API key`.
    pub prompt: &'static str,
    /// The answer is a credential: typed hidden.
    pub secret: bool,
    pub answer: Answer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignInHow {
    /// Run this where the user can answer it. `capture` is a token it prints for Nori to keep.
    Terminal {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        capture: Option<Capture>,
    },
    /// Ask the agent to sign in by itself (the method's id is the agent's own), with a secret in
    /// its environment for this sign-in when one was given.
    Agent { secret: Option<(String, Secret)> },
    /// Keep a credential the user gave in the account's Claude `settings.json`.
    StoreSetting {
        variable: &'static str,
        value: Secret,
    },
}

/// One way to sign in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignInMethod {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// Asked before the method runs; [`SignInMethod::answered`] folds the answer in.
    pub ask: Option<SignInAsk>,
    pub how: SignInHow,
}

impl SignInMethod {
    /// The method to run once its question is answered with `text`.
    pub fn answered(mut self, text: &str) -> Self {
        let text = text.trim();
        let Some(ask) = self.ask.take() else {
            return self;
        };
        match ask.answer {
            Answer::Arg(flag) => {
                if let SignInHow::Terminal { args, .. } = &mut self.how {
                    args.extend([flag.to_owned(), text.to_owned()]);
                }
            }
            Answer::AgentEnv(variable) => {
                if let SignInHow::Agent { secret } = &mut self.how {
                    *secret = Some((variable.to_owned(), Secret::new(text)));
                }
            }
            Answer::ClaudeSetting(_) => {
                if let SignInHow::StoreSetting { value, .. } = &mut self.how {
                    *value = Secret::new(text);
                }
            }
        }
        self
    }
}

fn method(id: &str, name: &str, description: &str, how: SignInHow) -> SignInMethod {
    SignInMethod {
        id: id.to_owned(),
        name: name.to_owned(),
        description: Some(description.to_owned()),
        ask: None,
        how,
    }
}

fn asking(
    mut method: SignInMethod,
    prompt: &'static str,
    secret: bool,
    answer: Answer,
) -> SignInMethod {
    method.ask = Some(SignInAsk {
        prompt,
        secret,
        answer,
    });
    method
}

/// `executable args` for `account`: the real program with the account's home when Nori runs the
/// account, the user's own command for the default login. `None` for a wrapper, and for a
/// launcher Nori cannot follow.
fn program(account: &Account, executable: &str, args: &[&str]) -> Option<LoginProgram> {
    let to_owned = |args: &[&str]| args.iter().map(|arg| (*arg).to_owned()).collect();
    match (&account.origin, &account.home) {
        (Origin::Native, Home::At(home)) => {
            let unwrapped = unwrap_executable(account.provider, executable)?;
            let mut env = unwrapped.env;
            env.extend(account.provider.environment(home));
            Some(LoginProgram {
                command: unwrapped.program.to_string_lossy().into_owned(),
                args: to_owned(args),
                env,
            })
        }
        (Origin::Default, _) => Some(LoginProgram {
            command: executable.to_owned(),
            args: to_owned(args),
            env: default_environment(account.provider),
        }),
        (Origin::Native, Home::Default) | (Origin::Wrapper { .. }, _) => None,
    }
}

fn terminal(program: LoginProgram, capture: Option<Capture>) -> SignInHow {
    SignInHow::Terminal {
        command: program.command,
        args: program.args,
        env: program.env,
        capture,
    }
}

/// The ways Nori itself offers to sign in to `slug`, beside what its agent offers
/// (`initialize`'s `authMethods`). Empty for a provider whose sign-in is only the agent's or a
/// provider Nori has no list for.
pub async fn methods_for(slug: &str) -> Vec<SignInMethod> {
    let Some(account) = super::find(slug) else {
        return Vec::new();
    };
    match account.provider.id {
        "claude" => claude(&account),
        "codex" => codex(&account),
        "opencode" => opencode(),
        "antigravity" => antigravity(),
        _ => Vec::new(),
    }
}

fn claude(account: &Account) -> Vec<SignInMethod> {
    let login = |flags: &[&str]| {
        let mut args = vec!["auth", "login"];
        args.extend_from_slice(flags);
        let (_, executable) = runs_executable(account.provider)?;
        program(account, &executable, &args)
    };
    let mut methods = Vec::new();
    if let Some(subscription) = login(&["--claudeai"]) {
        methods.push(method(
            "claudeai",
            "Claude subscription",
            "Pro, Max, Team or Enterprise · the browser signs you in; a code it shows is pasted here",
            terminal(subscription, None),
        ));
    }
    if let Some(with_email) = login(&["--claudeai"]) {
        methods.push(asking(
            method(
                "claudeai-email",
                "Claude subscription, with your email",
                "the sign-in page starts with your email filled in",
                terminal(with_email, None),
            ),
            "email",
            false,
            Answer::Arg("--email"),
        ));
    }
    if let Some(sso) = login(&["--claudeai", "--sso"]) {
        methods.push(asking(
            method(
                "sso",
                "Single sign-on (SSO)",
                "your organization's identity provider, from your work email",
                terminal(sso, None),
            ),
            "work email",
            false,
            Answer::Arg("--email"),
        ));
    }
    if let Some(console) = login(&["--console"]) {
        methods.push(method(
            "console",
            "Anthropic Console",
            "API usage billing instead of a subscription",
            terminal(console, None),
        ));
    }
    // A key or token is kept in the account's own settings; the default login's settings are
    // the user's own, so only an account Nori added gets one.
    if account.origin != Origin::Native {
        return methods;
    }
    if let Some((_, executable)) = runs_executable(account.provider)
        && let Some(setup) = program(account, &executable, &["setup-token"])
    {
        methods.push(method(
            "setup-token",
            "Long-lived token (sign in to make one)",
            "a one-year token for this account, kept in its settings",
            terminal(
                setup,
                Some(Capture {
                    prefix: "sk-ant-oat",
                    variable: "CLAUDE_CODE_OAUTH_TOKEN",
                }),
            ),
        ));
    }
    methods.push(asking(
        method(
            "token",
            "Long-lived token (paste one)",
            "from `claude setup-token` run anywhere",
            SignInHow::StoreSetting {
                variable: "CLAUDE_CODE_OAUTH_TOKEN",
                value: Secret::new(""),
            },
        ),
        "token",
        true,
        Answer::ClaudeSetting("CLAUDE_CODE_OAUTH_TOKEN"),
    ));
    methods.push(asking(
        method(
            "api-key",
            "Anthropic API key",
            "billed to the key's organization, kept in this account's settings",
            SignInHow::StoreSetting {
                variable: "ANTHROPIC_API_KEY",
                value: Secret::new(""),
            },
        ),
        "API key",
        true,
        Answer::ClaudeSetting("ANTHROPIC_API_KEY"),
    ));
    methods
}

fn codex(account: &Account) -> Vec<SignInMethod> {
    let mut methods = Vec::new();
    // Nori's own Codex cannot show a device code, so the Codex CLI does, for this account's home.
    if let Some(device) = program(account, "codex", &["login", "--device-auth"])
        && which::which(&device.command).is_ok()
    {
        methods.push(method(
            "device-code",
            "ChatGPT with a device code",
            "for a machine the browser's redirect cannot reach: enter the code it shows at chatgpt.com",
            terminal(device, None),
        ));
    }
    methods.push(asking(
        method(
            "codex-api-key",
            "OpenAI API key",
            "billed to the key's organization, kept by Codex in this account's home",
            SignInHow::Agent { secret: None },
        ),
        "API key",
        true,
        Answer::AgentEnv("CODEX_API_KEY"),
    ));
    methods
}

/// The official CLI's own sign-in (`agy acp --login`, the command it names when it needs one): a
/// browser, or over SSH an authorization URL and the code the browser shows, pasted back into the
/// panel. The CLI keeps the login in the OS keyring, for the user, not per account.
fn antigravity() -> Vec<SignInMethod> {
    let Ok(install) = crate::antigravity_cli::discover() else {
        return Vec::new();
    };
    vec![method(
        "google",
        "Google account",
        "the browser signs you in; over SSH, paste the code it shows here",
        SignInHow::Terminal {
            command: install.path.to_string_lossy().into_owned(),
            args: ["acp", "--login"].map(str::to_owned).to_vec(),
            env: HashMap::new(),
            capture: None,
        },
    )]
}

/// OpenCode's gateway takes an API key, which the agent keeps for the account.
fn opencode() -> Vec<SignInMethod> {
    vec![asking(
        method(
            "opencode-api-key",
            "OpenCode API key",
            "from opencode.ai/auth, billed to its workspace, kept by Nori for this account",
            SignInHow::Agent { secret: None },
        ),
        "API key",
        true,
        Answer::AgentEnv("OPENCODE_API_KEY"),
    )]
}

/// Keep `variable=value` in the `env` of the settings of the Claude account `slug`: where Claude
/// reads `ANTHROPIC_API_KEY` and `CLAUDE_CODE_OAUTH_TOKEN` from for that account's home. Only an
/// account Nori added has a home to write to.
pub fn store_claude_setting(slug: &str, variable: &str, value: &str) -> Result<()> {
    let account = super::find(slug).with_context(|| format!("{slug} is not an account"))?;
    anyhow::ensure!(
        account.provider.id == "claude" && account.origin == Origin::Native,
        "a key or token is kept only for a Claude account that Nori added"
    );
    let Home::At(home) = &account.home else {
        anyhow::bail!("the account has no home of its own");
    };
    write_setting(&home.join("settings.json"), variable, value)
}

fn write_setting(path: &Path, variable: &str, value: &str) -> Result<()> {
    let mut settings = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .filter(serde_json::Value::is_object)
            .with_context(|| {
                format!(
                    "{} is not a JSON object, and Nori will not overwrite it",
                    path.display()
                )
            })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", path.display()));
        }
    };
    let env = settings
        .as_object_mut()
        .context("settings are an object")?
        .entry("env")
        .or_insert_with(|| serde_json::json!({}));
    env.as_object_mut()
        .with_context(|| format!("`env` in {} is not an object", path.display()))?
        .insert(variable.to_owned(), serde_json::Value::from(value));
    let temporary = path.with_extension("json.nori-tmp");
    write_private(&temporary, &serde_json::to_vec_pretty(&settings)?)?;
    std::fs::rename(&temporary, path).with_context(|| format!("could not write {}", path.display()))
}

/// Write `bytes` to a new file only its owner can read.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .with_context(|| format!("could not write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    fn email_method() -> SignInMethod {
        asking(
            method(
                "claudeai-email",
                "x",
                "y",
                SignInHow::Terminal {
                    command: "claude".to_owned(),
                    args: vec!["auth".to_owned(), "login".to_owned()],
                    env: HashMap::new(),
                    capture: None,
                },
            ),
            "email",
            false,
            Answer::Arg("--email"),
        )
    }

    #[test]
    fn an_email_answer_becomes_the_programs_email_flag() {
        let answered = email_method().answered("  f@example.org ");
        assert_eq!(
            answered.how,
            SignInHow::Terminal {
                command: "claude".to_owned(),
                args: ["auth", "login", "--email", "f@example.org"]
                    .map(str::to_owned)
                    .to_vec(),
                env: HashMap::new(),
                capture: None,
            }
        );
        assert_eq!(answered.ask, None);
    }

    #[test]
    fn a_secret_answer_goes_to_the_agent_and_stays_out_of_debug_output() {
        let method = asking(
            method("codex-api-key", "x", "y", SignInHow::Agent { secret: None }),
            "API key",
            true,
            Answer::AgentEnv("CODEX_API_KEY"),
        )
        .answered("sk-proj-very-secret");
        assert_eq!(
            method.how,
            SignInHow::Agent {
                secret: Some((
                    "CODEX_API_KEY".to_owned(),
                    Secret::new("sk-proj-very-secret")
                ))
            }
        );
        assert!(!format!("{method:?}").contains("very-secret"));
    }

    #[test]
    fn storing_a_setting_keeps_the_rest_of_the_settings_and_the_file_private() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("settings.json");
        std::fs::write(&path, r#"{"model":"opus","env":{"FOO":"1"}}"#).expect("write");
        write_setting(&path, "ANTHROPIC_API_KEY", "sk-ant-fake").expect("stored");
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read")).expect("json");
        assert_eq!(
            written,
            serde_json::json!({
                "model": "opus",
                "env": { "FOO": "1", "ANTHROPIC_API_KEY": "sk-ant-fake" },
            })
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn storing_a_setting_creates_the_file_and_refuses_to_clobber_a_broken_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fresh = dir.path().join("fresh.json");
        write_setting(&fresh, "CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-fake").expect("stored");
        assert!(fresh.exists());
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "not json").expect("write");
        assert!(write_setting(&broken, "X", "y").is_err());
        assert_eq!(std::fs::read_to_string(&broken).expect("read"), "not json");
    }
}
