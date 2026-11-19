use super::*;

impl ChatWidget {
    pub(crate) fn external_cli_login_active(&self) -> bool {
        self.login_handler
            .as_ref()
            .is_some_and(LoginHandler::has_external_cli_task)
    }

    /// Give the external login terminal exclusive access to keyboard input.
    pub(crate) fn route_external_cli_login_key_event(&mut self, key: KeyEvent) -> bool {
        let Some(handler) = self.login_handler.as_ref() else {
            return false;
        };
        if !handler.has_external_cli_task() {
            return false;
        }
        if key.kind == KeyEventKind::Release {
            return true;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            if let Some(mut handler) = self.login_handler.take() {
                handler.cancel();
            }
            self.login_terminal = None;
            self.app_event_tx.send(AppEvent::LoginCancelled);
            self.login_agent_override = None;
            self.add_login_message("Login cancelled.".to_string(), None);
            self.login_child_target = None;
            self.request_redraw();
            return true;
        }

        // The sign-in panel edits what the user types itself and sends it whole; what it does
        // not take (a key for the program's own menu) goes to the program as it is.
        let bytes = match self
            .login_terminal
            .as_mut()
            .map(|terminal| terminal.key(key))
        {
            Some(crate::login_terminal::KeyOutcome::Send(bytes)) => bytes,
            Some(crate::login_terminal::KeyOutcome::Handled) => {
                self.request_redraw();
                return true;
            }
            Some(crate::login_terminal::KeyOutcome::Ignored) => Vec::new(),
            None => raw_key_bytes(key),
        };
        if !bytes.is_empty()
            && let Some(handler) = self.login_handler.as_ref()
        {
            handler.send_pty_input(bytes);
        }
        self.request_redraw();
        true
    }

    /// Forward pasted login input without placing it in the chat composer.
    pub(crate) fn route_external_cli_login_paste(&mut self, text: String) -> bool {
        let Some(handler) = self.login_handler.as_ref() else {
            return false;
        };
        if !handler.has_external_cli_task() {
            return false;
        }
        match self.login_terminal.as_mut() {
            // A paste lands in the panel's input line, whole, to be checked and sent with Enter.
            Some(terminal) => {
                terminal.paste(&text);
                self.request_redraw();
            }
            None => {
                handler.send_pty_input(text.into_bytes());
            }
        }
        true
    }
}

/// A key as a terminal program reads it.
fn raw_key_bytes(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(ch)
            if key.modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii_alphabetic() =>
        {
            vec![(ch.to_ascii_lowercase() as u8) - b'a' + 1]
        }
        KeyCode::Char(ch) => ch.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        _ => Vec::new(),
    }
}
