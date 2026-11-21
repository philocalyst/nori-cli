//! Signing in to a provider, the way its agent says.
//!
//! Nori asks the agent how it signs in (`nori_acp_host::sign_in`) and runs what it answers with
//! the agent's own launch command: the profile wrapper of an account, its environment, its home.
//! So every account (Claude and Claude 2, Codex and Codex 2) signs in where that account's agent
//! reads its credentials, and none of them can sign another in by accident. A terminal method
//! runs in the sign-in panel above the prompt; an agent method signs in by itself (in the
//! browser) while the panel waits. Either way "signed in" is said only once a fresh agent can
//! open a session; until then the provider is not signed in, whatever the program printed.
//!
//! Besides what its agent offers, an account is offered the provider's own ways
//! (`nori_acp_host::accounts::methods_for`: Claude's subscription, email, SSO, Console, token and
//! key; Codex's device code and key; OpenCode's login). A way that needs an answer asks first, in
//! the picker (an email; a key or token, typed as dots); a code a browser shows is pasted into the
//! sign-in panel's own input line. An agent that offers no method keeps the sign-in Nori knows for
//! it (`/login`'s own table), unless it is an account with a home of its own, which that table
//! cannot reach.
//!
//! Adding an account (`/providers`, "Add another account") creates it and goes straight on to
//! signing it in. The account is "signed in" only once its own login says so
//! (`nori_acp_host::accounts::status`); a login that was abandoned leaves it "not signed in".

use super::*;
use nori_acp_host::accounts::Capture;
use nori_acp_host::sign_in::SignInHow;
use nori_acp_host::sign_in::SignInMethod;
use nori_config::ProviderAccounts;

/// The provider a sign-in is for.
#[derive(Clone, Debug)]
pub(crate) struct SignInTarget {
    pub(crate) agent_name: String,
    pub(crate) display_name: String,
    /// A token the sign-in program prints, for Nori to keep once it ends (`claude setup-token`).
    pub(crate) capture: Option<Capture>,
}

impl ChatWidget {
    /// Sign in to `agent_name`: first ask its agent how.
    pub(crate) fn sign_in(&mut self, agent_name: &str) {
        // A CLI that is not there cannot say how it signs in: say what to install (and what else
        // works) instead of starting it through a package runner to find out.
        let missing = nori_harness::list_available_agents()
            .into_iter()
            .any(|agent| {
                agent.agent_name == agent_name
                    && !agent.is_installed
                    && nori_acp_host::accounts::install_command(agent.agent).is_some()
            });
        if missing {
            self.handle_login_command_with_agent(agent_name);
            return;
        }
        let Ok(config) = nori_harness::get_agent_config(agent_name) else {
            self.handle_login_command_with_agent(agent_name);
            return;
        };
        let target = SignInTarget {
            agent_name: agent_name.to_owned(),
            display_name: config.display_name.clone(),
            capture: None,
        };
        self.login_terminal = Some(crate::login_terminal::LoginTerminal::waiting(
            target.display_name.clone(),
            "asking how it signs in",
        ));
        self.request_redraw();
        let cwd = self.config.cwd.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let methods = nori_acp_host::sign_in::sign_in_methods(&config, &cwd)
                .await
                .map_err(|error| format!("{error:#}"));
            tx.send(AppEvent::SignInMethods { target, methods });
        });
    }

    /// The agent said how it signs in.
    pub(crate) fn on_sign_in_methods(
        &mut self,
        target: SignInTarget,
        methods: Result<Vec<SignInMethod>, String>,
    ) {
        self.login_terminal = None;
        match methods {
            Ok(mut methods) if methods.len() == 1 && methods[0].ask.is_none() => {
                if let Some(method) = methods.pop() {
                    self.run_sign_in(target, method);
                }
            }
            Ok(methods) if !methods.is_empty() => {
                let items = methods
                    .into_iter()
                    .map(|method| {
                        let target = target.clone();
                        let detail = method.description.clone().unwrap_or_default();
                        let label = method.name.clone();
                        let item = match method.ask.clone() {
                            // A way that needs an answer asks it first; the answer completes it.
                            Some(ask) => crate::choice_picker::ChoiceItem::ask(
                                label,
                                crate::choice_picker::Ask {
                                    verb: ask.prompt,
                                    label: target.display_name.clone(),
                                    initial: String::new(),
                                    secret: ask.secret,
                                    then: std::sync::Arc::new(move |text, tx| {
                                        tx.send(AppEvent::SignInWith {
                                            target: target.clone(),
                                            method: method.clone().answered(text),
                                        });
                                        Ok(crate::choice_picker::Next::Done)
                                    }),
                                },
                            ),
                            None => crate::choice_picker::ChoiceItem::run(
                                label,
                                Box::new(move |tx| {
                                    tx.send(AppEvent::SignInWith {
                                        target: target.clone(),
                                        method: method.clone(),
                                    })
                                }),
                            ),
                        };
                        item.detail(detail)
                    })
                    .collect();
                let mut spec = crate::choice_picker::ChoiceSpec::new(
                    format!("Sign in to {}", target.display_name),
                    items,
                );
                // Gemini's API key is read from its environment, not typed here: where it goes.
                let key_note = (target.agent_name == "gemini").then(|| {
                    format!(
                        " · an API key goes in {}",
                        nori_acp_host::accounts::gemini_cli::KEY_VARIABLE
                    )
                });
                spec.subtitle = Some(format!(
                    "choose how · it counts as signed in once its login is checked{}",
                    key_note.unwrap_or_default()
                ));
                self.app_event_tx
                    .send(AppEvent::ShowChoicePicker(Box::new(spec)));
            }
            // No method offered, or the agent could not be asked: the sign-in Nori knows. It runs
            // the provider's program where that keeps its own login, so it can only sign in an
            // account that lives there, never one with a home of its own.
            Ok(_) | Err(_) if nori_config::parse_account_slug(&target.agent_name).is_none() => {
                self.handle_login_command_with_agent(&target.agent_name);
            }
            Ok(_) => self.add_error_message(format!(
                "{} offers no sign-in Nori can run for this account",
                target.display_name
            )),
            Err(error) => self.add_error_message(format!(
                "Could not ask {} how it signs in: {error}",
                target.display_name
            )),
        }
    }

    /// Create the account `name` of `provider` (its home, its `[[accounts]]` entry, its agent) off
    /// the event loop; signing it in follows.
    pub(crate) fn add_account(&mut self, provider: &'static ProviderAccounts, name: String) {
        let nori_home = self.config.nori_home.clone();
        let tx = self.app_event_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = nori_acp_host::accounts::add_native_account(&nori_home, provider, &name)
                .map(|account| account.slug)
                .map_err(|error| format!("{error:#}"));
            tx.send(AppEvent::AccountAdded {
                provider,
                name,
                result,
            });
        });
    }

    /// The account exists: sign it in right away, so it works as soon as it is listed.
    pub(crate) fn on_account_added(
        &mut self,
        provider: &'static ProviderAccounts,
        name: &str,
        result: Result<String, String>,
    ) {
        match result {
            Ok(slug) => self.sign_in(&slug),
            Err(error) => self.add_error_message(format!(
                "Could not add the {} account {name}: {error}",
                provider.title
            )),
        }
    }

    /// Sign in with `method`.
    pub(crate) fn run_sign_in(&mut self, target: SignInTarget, method: SignInMethod) {
        match method.how {
            SignInHow::Terminal {
                command,
                args,
                env,
                capture,
            } => {
                let target = SignInTarget { capture, ..target };
                self.sign_in_target = Some(target.clone());
                let mut handler = LoginHandler::new();
                handler.start_external_cli(target.display_name.clone());
                self.start_external_cli_login_flow_with_env(
                    handler,
                    command,
                    args,
                    env,
                    target.display_name,
                );
            }
            SignInHow::Agent { secret } => {
                self.sign_in_target = Some(target.clone());
                self.login_terminal = Some(crate::login_terminal::LoginTerminal::waiting(
                    target.display_name.clone(),
                    if secret.is_some() {
                        "signing in with the key"
                    } else {
                        "finish signing in in your browser"
                    },
                ));
                self.request_redraw();
                let Ok(config) = nori_harness::get_agent_config(&target.agent_name) else {
                    return;
                };
                let cwd = self.config.cwd.clone();
                let tx = self.app_event_tx.clone();
                tokio::spawn(async move {
                    let result = match nori_acp_host::sign_in::authenticate(
                        &config,
                        &cwd,
                        &method.id,
                        secret.as_ref(),
                    )
                    .await
                    {
                        Ok(()) => nori_acp_host::sign_in::verify(&config, &cwd).await,
                        Err(error) => Err(error),
                    }
                    .map_err(|error| format!("{error:#}"));
                    tx.send(AppEvent::AccountChecked {
                        slug: target.agent_name.clone(),
                    });
                    tx.send(AppEvent::SignInVerified { target, result });
                });
            }
            SignInHow::StoreSetting { variable, value } => {
                self.sign_in_target = Some(target.clone());
                match nori_acp_host::accounts::store_claude_setting(
                    &target.agent_name,
                    variable,
                    value.expose(),
                ) {
                    Ok(()) => self.verify_sign_in(target),
                    Err(error) => {
                        self.sign_in_target = None;
                        self.add_error_message(format!(
                            "Could not keep the {} credential: {error:#}",
                            target.display_name
                        ));
                    }
                }
            }
        }
    }

    /// A terminal sign-in program ended well: check that it took before saying so.
    pub(crate) fn verify_sign_in(&mut self, target: SignInTarget) {
        let Ok(config) = nori_harness::get_agent_config(&target.agent_name) else {
            self.on_sign_in_verified(target, Ok(()));
            return;
        };
        self.login_terminal = Some(crate::login_terminal::LoginTerminal::waiting(
            target.display_name.clone(),
            "checking the sign-in took",
        ));
        self.request_redraw();
        let cwd = self.config.cwd.clone();
        let tx = self.app_event_tx.clone();
        tokio::spawn(async move {
            let result = nori_acp_host::sign_in::verify(&config, &cwd)
                .await
                .map_err(|error| format!("{error:#}"));
            tx.send(AppEvent::AccountChecked {
                slug: target.agent_name.clone(),
            });
            tx.send(AppEvent::SignInVerified { target, result });
        });
    }

    /// Whether the sign-in took, as a fresh agent found.
    pub(crate) fn on_sign_in_verified(&mut self, target: SignInTarget, result: Result<(), String>) {
        self.login_terminal = None;
        self.sign_in_target = None;
        match result {
            Ok(()) => {
                // The check that passed also said who it is.
                let who = nori_acp_host::accounts::identity_of(&target.agent_name)
                    .map(|identity| identity.describe());
                self.add_login_message(
                    match &who {
                        Some(who) => format!("Signed in to {} as {who}", target.display_name),
                        None => format!("Signed in to {}", target.display_name),
                    },
                    Some("its models are in /model and /providers".to_owned()),
                );
                if target.agent_name == self.config.active_agent && !self.has_harness_session() {
                    self.app_event_tx.send(AppEvent::NewSession);
                }
            }
            Err(error) => self.add_error_message(format!(
                "{} is still not signed in: {error}",
                target.display_name
            )),
        }
        self.request_redraw();
    }
}
