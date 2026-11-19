use super::*;

use std::path::Path;

impl ChatWidget {
    pub(super) fn handle_explicit_login(&mut self, agent_name: &str) {
        if self.focused_subagent.is_none() {
            // Signed in through the agent itself, so the account's own home gets the login.
            self.sign_in(agent_name);
            return;
        }
        self.login_child_target = self.focused_subagent;
        self.handle_login_command_with_agent(agent_name);
        if self.login_handler.is_none() {
            self.login_child_target = None;
        }
    }
    pub(super) fn add_login_message(&mut self, message: String, hint: Option<String>) {
        if let Some(key) = self.login_child_target {
            let mut text = message;
            if let Some(hint) = hint {
                text.push('\n');
                text.push_str(&hint);
            }
            text.push('\n');
            self.app_event_tx
                .send(AppEvent::ChildSessionOutput { key, text });
        } else {
            self.add_info_message(message, hint);
        }
    }

    fn add_login_error(&mut self, message: String) {
        if self.login_child_target.is_some() {
            self.add_login_message(message, None);
        } else {
            self.add_error_message(message);
        }
    }

    /// Handle the /login slash command
    pub(super) fn handle_login_command(&mut self) {
        if let Some(key) = self.focused_subagent {
            let Some(agent_name) = self.subagents.agent_for_key(key).map(str::to_owned) else {
                self.app_event_tx.send(AppEvent::TransientNotice(
                    "This child does not identify its provider; login is unavailable.".to_owned(),
                ));
                return;
            };
            self.login_child_target = Some(key);
            self.handle_login_command_with_agent(&agent_name);
            if self.login_handler.is_none() {
                self.login_child_target = None;
            }
            return;
        }
        self.login_child_target = None;
        let agent_name = self.login_target_agent().to_string();
        self.sign_in(&agent_name);
    }

    pub(super) fn login_target_agent(&self) -> &str {
        self.login_agent_override
            .as_deref()
            .unwrap_or(&self.config.active_agent)
    }

    pub(crate) fn set_login_agent_override(&mut self, agent_name: Option<String>) {
        self.login_agent_override = agent_name;
    }

    /// Handle the /login <agent> command with explicit agent name
    pub(crate) fn handle_login_command_with_agent(&mut self, agent_name: &str) {
        match LoginHandler::check_agent_support(agent_name) {
            AgentLoginSupport::Supported {
                agent,
                is_installed,
                login_method,
            } => {
                if !is_installed {
                    let display_name = agent.display_name();
                    let npm_package = agent.npm_package();
                    self.add_login_message(
                        format!(
                            "{display_name} is not installed. To install, run:\n\n  npm install -g {npm_package}\n\nThen run /login again to authenticate."
                        ),
                        Some("Install the agent first, then authenticate".to_string()),
                    );
                    return;
                }

                match login_method {
                    LoginMethod::OAuthBrowser => {
                        let mut handler = LoginHandler::new();
                        handler.start_oauth();

                        self.add_login_message(
                            "Starting authentication...\n\nA browser window will open for you to sign in with your OpenAI account.\n\nAlternatively, you can set the OPENAI_API_KEY environment variable.".to_string(),
                            Some("Press Esc to cancel".to_string()),
                        );

                        self.start_oauth_login_flow(handler);
                    }
                    LoginMethod::ExternalCli { command, args } => {
                        let mut handler = LoginHandler::new();
                        let agent_display_name = agent.display_name().to_string();
                        handler.start_external_cli(agent_display_name.clone());

                        // The sign-in panel above the prompt says what is happening.
                        self.start_external_cli_login_flow(
                            handler,
                            command,
                            args,
                            agent_display_name,
                        );
                    }
                }
            }
            AgentLoginSupport::Unknown { agent_name } => {
                self.add_login_message(
                    format!("Unknown agent '{agent_name}'. Cannot determine login method."),
                    None,
                );
            }
        }
    }

    /// Start the OAuth login flow
    pub(super) fn start_oauth_login_flow(&mut self, mut handler: LoginHandler) {
        use codex_login::CLIENT_ID;
        use codex_login::ServerOptions;
        use codex_login::run_login_server;

        let credentials_home =
            codex_oauth_credentials_home(&self.config.nori_home, dirs::home_dir());
        let opts = ServerOptions::new(
            credentials_home,
            CLIENT_ID.to_string(),
            None, // No forced workspace ID
            codex_login::AuthCredentialsStoreMode::File,
            codex_login::AuthKeyringBackendKind::default(),
            codex_login::AuthRouteConfig::from_http_client_factory(
                codex_http_client::HttpClientFactory::new(
                    codex_http_client::OutboundProxyPolicy::ReqwestDefault,
                ),
            ),
        );

        match run_login_server(opts) {
            Ok(child) => {
                let auth_url = child.auth_url.clone();
                handler.set_shutdown_handle(child.cancel_handle());

                // Store the handler
                self.login_handler = Some(handler);
                self.app_event_tx.send(AppEvent::LoginStarted {
                    target: self.login_child_target,
                });

                // Update the info message with the URL
                self.add_login_message(
                    format!(
                        "Opening browser for authentication...\n\nIf the browser doesn't open automatically, visit:\n{auth_url}\n\nWaiting for authentication to complete..."
                    ),
                    Some("Press Esc to cancel".to_string()),
                );

                // Spawn a task to wait for completion
                let app_event_tx = self.app_event_tx.clone();
                let auth_manager = self.auth_manager.clone();
                tokio::spawn(async move {
                    match child.block_until_done().await {
                        Ok(()) => {
                            auth_manager.reload().await;
                            app_event_tx.send(AppEvent::LoginComplete { success: true });
                        }
                        Err(e) => {
                            tracing::error!("OAuth login failed: {e}");
                            app_event_tx.send(AppEvent::LoginComplete { success: false });
                        }
                    }
                });
            }
            Err(e) => {
                self.add_login_error(format!("Failed to start login server: {e}"));
            }
        }
    }

    /// Start the external CLI login flow (e.g., gemini login)
    #[cfg(feature = "login")]
    pub(super) fn start_external_cli_login_flow(
        &mut self,
        handler: LoginHandler,
        command: String,
        args: Vec<String>,
        agent_display_name: String,
    ) {
        self.start_external_cli_login_flow_with_env(
            handler,
            command,
            args,
            std::collections::HashMap::new(),
            agent_display_name,
        );
    }

    /// Run a sign-in program in the panel, with `extra_env` over Nori's own environment (an
    /// agent's launch environment: the home of the account it signs in to).
    pub(super) fn start_external_cli_login_flow_with_env(
        &mut self,
        mut handler: LoginHandler,
        command: String,
        args: Vec<String>,
        extra_env: std::collections::HashMap<String, String>,
        agent_display_name: String,
    ) {
        use std::collections::HashMap;

        let app_event_tx = self.app_event_tx.clone();
        let cwd = self.config.cwd.clone();
        // The program draws into a panel above the prompt, on a screen wide enough that it never
        // cuts a URL; the panel shows the URL as a link instead.
        self.login_terminal = Some(crate::login_terminal::LoginTerminal::new(
            agent_display_name.clone(),
        ));
        // Install the queue before spawning so input entered immediately is retained. It is deep
        // enough that nothing typed or pasted is ever dropped.
        let (input_tx, mut input_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4096);
        handler.set_pty_input_sender(input_tx);

        // Spawn the PTY process and stream output
        let task_handle = tokio::spawn(async move {
            // Build environment - inherit current environment
            let mut env: HashMap<String, String> = std::env::vars().collect();
            env.extend(extra_env);
            // Ensure TERM is set for proper terminal behavior
            env.entry("TERM".to_string())
                .or_insert_with(|| "xterm-256color".to_string());

            match codex_utils_pty::spawn_pty_process_sized(
                &command,
                &args,
                &cwd,
                &env,
                &None,
                crate::login_terminal::ROWS,
                crate::login_terminal::COLS,
            )
            .await
            {
                Ok(spawned) => {
                    // Keep session alive so process keeps running
                    let session = spawned.session;
                    let writer_tx = session.writer_sender();
                    let mut output_rx = spawned.output_rx;
                    let mut exit_rx = spawned.exit_rx;

                    // Spawn a task to stream output
                    let output_event_tx = app_event_tx.clone();
                    let mut output_task = tokio::spawn(async move {
                        loop {
                            match output_rx.recv().await {
                                Ok(data) => {
                                    output_event_tx.send(AppEvent::ExternalCliLoginOutput { data });
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                    // Receiver lagged, continue
                                    continue;
                                }
                            }
                        }
                    });

                    // Wait for process exit
                    let exit_code = loop {
                        tokio::select! {
                            result = &mut exit_rx => break result.unwrap_or(-1),
                            input = input_rx.recv() => match input {
                                Some(bytes) => {
                                    if writer_tx.send(bytes).await.is_err() {
                                        break -1;
                                    }
                                }
                                None => break -1,
                            },
                        }
                    };

                    // Allow the PTY reader to deliver output written immediately before exit.
                    // The session itself holds an output sender, so the stream does not close
                    // until the session is dropped.
                    let _ =
                        tokio::time::timeout(Duration::from_millis(100), &mut output_task).await;
                    output_task.abort();

                    // Send completion event
                    let success = exit_code == 0;
                    app_event_tx.send(AppEvent::ExternalCliLoginComplete {
                        success,
                        agent_name: agent_display_name,
                    });
                }
                Err(e) => {
                    tracing::error!("Failed to spawn external CLI login: {e}");
                    app_event_tx.send(AppEvent::ExternalCliLoginComplete {
                        success: false,
                        agent_name: agent_display_name,
                    });
                }
            }
        });

        // Store the task handle for cancellation support
        handler.set_pty_task_handle(task_handle);
        self.login_handler = Some(handler);
        self.app_event_tx.send(AppEvent::LoginStarted {
            target: self.login_child_target,
        });
    }

    /// Start the external CLI login flow (stub for non-login builds)
    #[cfg(not(feature = "login"))]
    pub(super) fn start_external_cli_login_flow(
        &mut self,
        _handler: LoginHandler,
        _command: String,
        _args: Vec<String>,
        _agent_display_name: String,
    ) {
        self.add_login_error(
            "Login feature is not enabled. Rebuild with --features login".to_string(),
        );
    }

    /// Handle login completion event
    pub(crate) fn handle_login_complete(&mut self, success: bool) {
        if let Some(mut handler) = self.login_handler.take() {
            if success {
                self.login_agent_override = None;
                handler.oauth_complete();
                self.add_login_message(
                    "Successfully authenticated with OpenAI!\n\nYou can now use Nori.".to_string(),
                    None,
                );
                // The startup preparation ran before these credentials existed. Start a session
                // now so the next command, `/model` included, has an agent to ask.
                if !self.has_harness_session()
                    && nori_harness::AgentKind::from_slug(&self.config.active_agent)
                        == Some(nori_harness::AgentKind::Codex)
                {
                    self.app_event_tx.send(AppEvent::NewSession);
                }
            } else {
                handler.cancel();
                self.add_login_message("Login cancelled or failed.".to_string(), None);
            }
        }
        self.login_child_target = None;
        self.request_redraw();
    }

    /// Bytes the sign-in program wrote: its screen, kept whole in the panel above the prompt.
    pub(crate) fn handle_external_cli_login_output(&mut self, data: Vec<u8>) {
        if !self
            .login_handler
            .as_ref()
            .is_some_and(LoginHandler::has_external_cli_task)
        {
            return;
        }
        if let Some(key) = self.login_child_target {
            let text = strip_ansi_codes(&String::from_utf8_lossy(&data));
            if !text.is_empty() {
                self.app_event_tx
                    .send(AppEvent::ChildSessionOutput { key, text });
            }
        } else if let Some(terminal) = self.login_terminal.as_mut() {
            terminal.feed(&data);
        }
        self.request_redraw();
    }

    /// Handle external CLI login completion
    pub(crate) fn handle_external_cli_login_complete(&mut self, success: bool, agent_name: String) {
        let Some(mut handler) = self.login_handler.take() else {
            return;
        };
        handler.cancel(); // Clear any handler state
        // The panel goes; what the program last said is the reason a failure is given.
        let last_words = self
            .login_terminal
            .take()
            .map(|terminal| terminal.last_words(3))
            .unwrap_or_default();

        // A sign-in run through the agent is said to have worked only once the agent starts.
        if success && let Some(target) = self.sign_in_target.clone() {
            self.login_agent_override = None;
            self.login_child_target = None;
            self.verify_sign_in(target);
            return;
        }
        if !success && let Some(target) = self.sign_in_target.take() {
            let reason = (!last_words.is_empty()).then(|| last_words.join(" · "));
            self.add_login_message(
                format!("{} sign-in did not finish", target.display_name),
                reason,
            );
            self.login_child_target = None;
            self.request_redraw();
            return;
        }
        if success {
            self.login_agent_override = None;
            self.add_login_message(
                format!("Signed in to {agent_name}"),
                Some("its models are in /model and /providers".to_owned()),
            );
            if agent_name == "Claude Code"
                && nori_harness::AgentKind::from_slug(&self.config.active_agent)
                    == Some(nori_harness::AgentKind::ClaudeCode)
                && !self.has_harness_session()
            {
                self.app_event_tx.send(AppEvent::NewSession);
            }
        } else {
            let reason = (!last_words.is_empty()).then(|| last_words.join(" · "));
            self.add_login_message(format!("{agent_name} sign-in did not finish"), reason);
        }
        self.login_child_target = None;
        self.request_redraw();
    }
}

/// Directory the Codex OAuth login writes credentials to.
///
/// This must match the directory the codex-acp subprocess reads auth from. That
/// subprocess is spawned with `CODEX_HOME` stripped (see the connection spawn in
/// `nori-acp-host`), so it resolves its home to `$HOME/.codex`. Writing anywhere
/// else (for example `nori_home`) means the agent never sees the credentials.
/// Falls back to `nori_home` only when the home directory cannot be determined.
fn codex_oauth_credentials_home(nori_home: &Path, home_dir: Option<PathBuf>) -> PathBuf {
    home_dir
        .map(|home| home.join(".codex"))
        .unwrap_or_else(|| nori_home.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::codex_oauth_credentials_home;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    #[test]
    fn oauth_login_targets_codex_agent_home_when_home_known() {
        let nori_home = PathBuf::from("/example/home/.nori/cli");
        let home_dir = PathBuf::from("/example/home");

        let destination = codex_oauth_credentials_home(&nori_home, Some(home_dir));

        assert_eq!(destination, PathBuf::from("/example/home/.codex"));
    }

    #[test]
    fn oauth_login_falls_back_to_nori_home_when_home_unknown() {
        let nori_home = PathBuf::from("/example/home/.nori/cli");

        let destination = codex_oauth_credentials_home(&nori_home, None);

        assert_eq!(destination, nori_home);
    }
}
