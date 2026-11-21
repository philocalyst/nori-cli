use std::sync::LazyLock;

use super::*;
use nori_config::HotkeyBinding;
use nori_config::VimEnterBehavior;

/// Older effort bindings, kept working where a terminal reports Shift on a letter.
static NEXT_EFFORT_ALIAS: LazyLock<HotkeyBinding> =
    LazyLock::new(|| HotkeyBinding::from_str("ctrl+shift+n"));
static PREVIOUS_EFFORT_ALIAS: LazyLock<HotkeyBinding> =
    LazyLock::new(|| HotkeyBinding::from_str("ctrl+shift+p"));

impl ChatComposer {
    /// Handle a key event coming from the main UI.
    pub fn handle_key_event(&mut self, key_event: KeyEvent) -> (InputResult, bool) {
        if !self.input_enabled {
            return (InputResult::None, false);
        }
        // The editor's search/select prompt owns every key while it is open (Enter runs it,
        // Esc closes it): nothing is submitted or walked from under it.
        if self.textarea.mode() == EditorMode::Prompt {
            if key_event.kind != KeyEventKind::Release {
                self.editor_input(key_event);
            }
            return (InputResult::None, true);
        }
        if key_event.code == KeyCode::Enter
            && key_event.modifiers == KeyModifiers::SHIFT
            && !self.popup_active()
        {
            if key_event.kind == KeyEventKind::Release {
                return (InputResult::None, true);
            }
            let (result, redraw) = self.handle_key_event(KeyEvent {
                modifiers: KeyModifiers::NONE,
                ..key_event
            });
            return (
                match result {
                    InputResult::Submitted(text) => InputResult::Steer(text),
                    InputResult::PromoteQueued => InputResult::None,
                    other => other,
                },
                redraw,
            );
        }
        let is_newline_shortcut = self.walk.is_none()
            && !matches!(self.active_popup, ActivePopup::HistorySearch(_))
            && matches!(
                key_event,
                KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::ALT,
                    ..
                }
            );
        // Enter on a walked-back prompt rewinds to it, whatever popup its text opened.
        let walk_enter = self.walk.is_some()
            && key_event.code == KeyCode::Enter
            && matches!(key_event.modifiers, KeyModifiers::NONE | KeyModifiers::ALT);
        let result = if walk_enter && let Some(result) = self.handle_walk_key(key_event) {
            result
        } else if is_newline_shortcut {
            if key_event.kind == KeyEventKind::Release {
                (InputResult::None, true)
            } else if self.textarea.mode() != EditorMode::Insert {
                self.textarea.editor_mut().cancel_pending();
                self.textarea.insert_str("\n");
                (InputResult::None, true)
            } else {
                // Route through normal input handling so a newline arriving
                // during a non-bracketed paste stays ordered with its buffer.
                self.handle_input_basic(KeyEvent {
                    code: KeyCode::Enter,
                    modifiers: KeyModifiers::NONE,
                    ..key_event
                })
            }
        } else {
            match &mut self.active_popup {
                ActivePopup::Command(_) => self.handle_key_event_with_slash_popup(key_event),
                ActivePopup::Skill(_) => self.handle_key_event_with_skill_popup(key_event),
                ActivePopup::File(_) => self.handle_key_event_with_file_popup(key_event),
                ActivePopup::HistorySearch(_) => {
                    self.handle_key_event_with_history_popup(key_event)
                }
                ActivePopup::Palette(_) => self.handle_key_event_with_palette(key_event),
                ActivePopup::None => self.handle_key_event_without_popup(key_event),
            }
        };

        if matches!(
            &result.0,
            InputResult::Submitted(_) | InputResult::Command(_)
        ) {
            // A recalled entry of an earlier session was sent: the draft the walk began from
            // comes back.
            if self.walk.is_some() {
                // What was attached while walking goes with the recalled prompt; the stashed
                // draft's attachments come back after the widget took those.
                let sent = std::mem::take(&mut self.attached_images);
                self.end_walk_quietly();
                self.held_images = std::mem::replace(&mut self.attached_images, sent);
            }
            // The next prompt starts ready to type.
            self.textarea.enter_insert();
        }

        self.sync_selection_popups();

        result
    }

    /// Handle key event when the slash-command popup is visible.
    pub(super) fn handle_key_event_with_slash_popup(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        let in_vim_normal = self.textarea.is_normal_mode();
        if self.should_handle_vim_insert_escape(key_event) {
            return self.handle_input_basic(key_event);
        }
        if in_vim_normal && key_event.code == KeyCode::Esc {
            self.dismissed_command_popup_text = Some(
                self.textarea
                    .text()
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
            );
            self.active_popup = ActivePopup::None;
            return (InputResult::None, true);
        }
        if key_event.code != KeyCode::Esc {
            self.footer_mode = reset_mode_after_activity(self.footer_mode);
        }
        let ActivePopup::Command(popup) = &mut self.active_popup else {
            unreachable!();
        };

        match key_event {
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            } if in_vim_normal => {
                popup.move_up();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_up();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
                ..
            } if in_vim_normal => {
                popup.move_down();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_down();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                // Dismiss the slash popup; keep the current input untouched.
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Char('i'),
                modifiers: KeyModifiers::NONE,
                ..
            } if in_vim_normal => self.handle_input_basic(key_event),
            KeyEvent {
                code: KeyCode::Tab, ..
            } => {
                // Ensure popup filtering/selection reflects the latest composer text
                // before applying completion.
                let first_line = self.textarea.text().lines().next().unwrap_or("");
                popup.on_composer_text_change(first_line.to_string());
                if let Some(sel) = popup.selected_item() {
                    let mut cursor_target: Option<usize> = None;
                    match sel {
                        CommandItem::Builtin(cmd) => {
                            let starts_with_cmd = first_line
                                .trim_start()
                                .starts_with(&format!("/{}", cmd.command()));
                            if !starts_with_cmd {
                                self.textarea.set_text(&format!("/{} ", cmd.command()));
                            }
                            if !self.textarea.text().is_empty() {
                                cursor_target = Some(self.textarea.text().len());
                            }
                        }
                        CommandItem::UserPrompt(idx) => {
                            if let Some(prompt) = popup.prompt(idx) {
                                match prompt_selection_action(
                                    prompt,
                                    first_line,
                                    PromptSelectionMode::Completion,
                                ) {
                                    PromptSelectionAction::Insert { text, cursor } => {
                                        let target = cursor.unwrap_or(text.len());
                                        self.textarea.set_text(&text);
                                        cursor_target = Some(target);
                                    }
                                    PromptSelectionAction::Submit { .. } => {}
                                }
                            }
                        }
                        CommandItem::AgentCommand(idx) => {
                            if let Some(cmd) = popup.agent_command(idx) {
                                let display_name = if self.agent_command_prefix.is_empty() {
                                    cmd.name.clone()
                                } else {
                                    format!("{}:{}", self.agent_command_prefix, cmd.name)
                                };
                                self.textarea.set_text(&format!("/{display_name} "));
                                cursor_target = Some(self.textarea.text().len());
                            }
                        }
                    }
                    if let Some(pos) = cursor_target {
                        self.textarea.set_cursor(pos);
                    }
                }
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // If the current line starts with a custom prompt name and includes
                // positional args for a numeric-style template, expand and submit
                // immediately regardless of the popup selection.
                let first_line = self.textarea.text().lines().next().unwrap_or("");
                if let Some((name, _rest)) = parse_slash_name(first_line)
                    && let Some(prompt_name) = name.strip_prefix(&format!("{PROMPTS_CMD_PREFIX}:"))
                    && let Some(prompt) = self.custom_prompts.iter().find(|p| p.name == prompt_name)
                    && let Some(expanded) =
                        expand_if_numeric_with_positional_args(prompt, first_line)
                {
                    self.textarea.set_text("");
                    return (InputResult::Submitted(expanded), true);
                }

                if let Some(sel) = popup.selected_item() {
                    match sel {
                        CommandItem::Builtin(cmd) => {
                            self.textarea.set_text("");
                            return (InputResult::Command(cmd), true);
                        }
                        CommandItem::UserPrompt(idx) => {
                            if let Some(prompt) = popup.prompt(idx) {
                                if matches!(prompt.kind, CustomPromptKind::Script { .. }) {
                                    let args = extract_positional_args_for_prompt_line(
                                        first_line,
                                        &prompt.name,
                                    );
                                    self.app_event_tx.send(AppEvent::ExecuteScript {
                                        prompt: prompt.clone(),
                                        args,
                                    });
                                    self.textarea.set_text("");
                                    return (InputResult::None, true);
                                }
                                match prompt_selection_action(
                                    prompt,
                                    first_line,
                                    PromptSelectionMode::Submit,
                                ) {
                                    PromptSelectionAction::Submit { text } => {
                                        self.textarea.set_text("");
                                        return (InputResult::Submitted(text), true);
                                    }
                                    PromptSelectionAction::Insert { text, cursor } => {
                                        let target = cursor.unwrap_or(text.len());
                                        self.textarea.set_text(&text);
                                        self.textarea.set_cursor(target);
                                        return (InputResult::None, true);
                                    }
                                }
                            }
                            return (InputResult::None, true);
                        }
                        CommandItem::AgentCommand(idx) => {
                            if let Some(cmd) = popup.agent_command(idx) {
                                let text = format!("/{}", cmd.name);
                                self.textarea.set_text("");
                                return (InputResult::Submitted(text), true);
                            }
                            return (InputResult::None, true);
                        }
                    }
                }
                // Fallback to default newline handling if no command selected.
                self.handle_key_event_without_popup(key_event)
            }
            _ if in_vim_normal => (InputResult::None, true),
            input => self.handle_input_basic(input),
        }
    }

    /// Handle key event when the skill popup is visible.
    pub(super) fn handle_key_event_with_skill_popup(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        if self.should_handle_vim_insert_escape(key_event) {
            self.active_popup = ActivePopup::None;
            return self.handle_input_basic(key_event);
        }
        if key_event.code != KeyCode::Esc {
            self.footer_mode = reset_mode_after_activity(self.footer_mode);
        }

        match key_event {
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let ActivePopup::Skill(popup) = &mut self.active_popup else {
                    unreachable!();
                };
                popup.move_up();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                let ActivePopup::Skill(popup) = &mut self.active_popup else {
                    unreachable!();
                };
                popup.move_down();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                if let Some(token) = Self::current_dollar_token(&self.textarea) {
                    self.dismissed_skill_popup_token = Some(token.query);
                }
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let selected = match &self.active_popup {
                    ActivePopup::Skill(popup) => popup.selected_item(),
                    ActivePopup::None
                    | ActivePopup::Command(_)
                    | ActivePopup::File(_)
                    | ActivePopup::HistorySearch(_)
                    | ActivePopup::Palette(_) => None,
                };
                if let Some(item) = selected {
                    self.insert_selected_skill(&item.insert_text);
                    if let Some(query) = item.insert_text.strip_prefix('$') {
                        self.dismissed_skill_popup_token = Some(query.to_string());
                    }
                }
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            input => self.handle_input_basic(input),
        }
    }

    #[inline]
    pub(super) fn clamp_to_char_boundary(text: &str, pos: usize) -> usize {
        let mut p = pos.min(text.len());
        if p < text.len() && !text.is_char_boundary(p) {
            p = text
                .char_indices()
                .map(|(i, _)| i)
                .take_while(|&i| i <= p)
                .last()
                .unwrap_or(0);
        }
        p
    }

    #[inline]
    pub(super) fn handle_non_ascii_char(&mut self, input: KeyEvent) -> (InputResult, bool) {
        if let KeyEvent {
            code: KeyCode::Char(ch),
            ..
        } = input
        {
            let now = Instant::now();
            if self.paste_burst.try_append_char_if_active(ch, now) {
                return (InputResult::None, true);
            }
        }
        if let Some(pasted) = self.paste_burst.flush_before_modified_input() {
            self.handle_paste(pasted);
        }
        self.editor_input(input);
        (InputResult::None, true)
    }

    /// Handle key events when file search popup is visible.
    pub(super) fn handle_key_event_with_file_popup(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        if self.should_handle_vim_insert_escape(key_event) {
            self.active_popup = ActivePopup::None;
            return self.handle_input_basic(key_event);
        }
        if key_event.code != KeyCode::Esc {
            self.footer_mode = reset_mode_after_activity(self.footer_mode);
        }
        let ActivePopup::File(popup) = &mut self.active_popup else {
            unreachable!();
        };

        match key_event {
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_up();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_down();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                // Hide popup without modifying text, remember token to avoid immediate reopen.
                if let Some(tok) = Self::current_at_token(&self.textarea) {
                    self.dismissed_file_popup_token = Some(tok);
                }
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                let Some(sel) = popup.selected_match() else {
                    self.active_popup = ActivePopup::None;
                    return (InputResult::None, true);
                };

                let sel_path = sel.to_string();
                // If selected path looks like an image (png/jpeg), attach as image instead of inserting text.
                let is_image = Self::is_image_path(&sel_path);
                if is_image {
                    // Determine dimensions; if that fails fall back to normal path insertion.
                    let path_buf = PathBuf::from(&sel_path);
                    if let Ok((w, h)) = image::image_dimensions(&path_buf) {
                        // Remove the current @token (mirror logic from insert_selected_path without inserting text)
                        // using the flat text and byte-offset cursor API.
                        let cursor_offset = self.textarea.cursor();
                        let text = self.textarea.text();
                        // Clamp to a valid char boundary to avoid panics when slicing.
                        let safe_cursor = Self::clamp_to_char_boundary(text, cursor_offset);
                        let before_cursor = &text[..safe_cursor];
                        let after_cursor = &text[safe_cursor..];

                        // Determine token boundaries in the full text.
                        let start_idx = before_cursor
                            .char_indices()
                            .rfind(|(_, c)| c.is_whitespace())
                            .map(|(idx, c)| idx + c.len_utf8())
                            .unwrap_or(0);
                        let end_rel_idx = after_cursor
                            .char_indices()
                            .find(|(_, c)| c.is_whitespace())
                            .map(|(idx, _)| idx)
                            .unwrap_or(after_cursor.len());
                        let end_idx = safe_cursor + end_rel_idx;

                        self.textarea.replace_range(start_idx..end_idx, "");
                        self.textarea.set_cursor(start_idx);

                        let format_label = match Path::new(&sel_path)
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(str::to_ascii_lowercase)
                        {
                            Some(ext) if ext == "png" => "PNG",
                            Some(ext) if ext == "jpg" || ext == "jpeg" => "JPEG",
                            _ => "IMG",
                        };
                        self.attach_image(path_buf, w, h, format_label);
                        // Add a trailing space to keep typing fluid.
                        self.textarea.insert_str(" ");
                    } else {
                        // Fallback to plain path insertion if metadata read fails.
                        self.insert_selected_path(&sel_path);
                    }
                } else {
                    // Non-image: inserting file path.
                    self.insert_selected_path(&sel_path);
                }
                // No selection: treat Enter as closing the popup/session.
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            input => self.handle_input_basic(input),
        }
    }

    /// Handle key events when the history search popup is visible.
    pub(super) fn handle_key_event_with_history_popup(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        let ActivePopup::HistorySearch(popup) = &mut self.active_popup else {
            unreachable!();
        };

        let search_active = popup.is_search_active();

        match key_event {
            KeyEvent {
                code: KeyCode::Esc, ..
            } if search_active => {
                popup.deactivate_search();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.active_popup = ActivePopup::None;
                (InputResult::None, true)
            }
            // Enter: select the highlighted entry and close
            KeyEvent {
                code: KeyCode::Enter,
                ..
            } => {
                let selected = popup.selected_text().map(String::from);
                self.active_popup = ActivePopup::None;
                if let Some(text) = selected {
                    self.set_history_content(text);
                }
                (InputResult::None, true)
            }
            // Arrow and readline bindings navigate in either mode.
            KeyEvent {
                code: KeyCode::Up, ..
            }
            | KeyEvent {
                code: KeyCode::Char('p'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_up();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            }
            | KeyEvent {
                code: KeyCode::Char('n'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                popup.move_down();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Char('f'),
                modifiers,
                ..
            } if !search_active && modifiers == KeyModifiers::CONTROL => {
                popup.activate_search();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Char('f' | '/'),
                modifiers: KeyModifiers::NONE,
                ..
            } if !search_active => {
                popup.activate_search();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Char('k'),
                modifiers: KeyModifiers::NONE,
                ..
            } if !search_active => {
                popup.move_up();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::NONE,
                ..
            } if !search_active => {
                popup.move_down();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Backspace,
                ..
            } if search_active => {
                popup.pop_char();
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } if search_active && !has_ctrl_or_alt(modifiers) => {
                popup.push_char(ch);
                (InputResult::None, true)
            }
            // All other keys: consume without action
            _ => (InputResult::None, true),
        }
    }

    /// Handle key event when no popup is visible.
    pub(super) fn handle_key_event_without_popup(
        &mut self,
        key_event: KeyEvent,
    ) -> (InputResult, bool) {
        if let Some(result) = self.handle_walk_key(key_event) {
            return result;
        }
        if self.is_palette_key(&key_event) {
            self.open_palette();
            return (InputResult::None, true);
        }
        if self.should_handle_vim_insert_escape(key_event) {
            return self.handle_input_basic(key_event);
        }
        if self.is_shell_mode
            && self.textarea.text().is_empty()
            && matches!(key_event.code, KeyCode::Esc | KeyCode::Backspace)
        {
            self.is_shell_mode = false;
            return (InputResult::None, true);
        }
        // `?` on an empty composer lists the keys; `?` or Esc closes the list.
        if key_event.kind == KeyEventKind::Press
            && !self.is_shell_mode
            && (matches!(self.footer_mode, FooterMode::ShortcutOverlay)
                && matches!(key_event.code, KeyCode::Esc | KeyCode::Char('?'))
                || self.is_empty()
                    && key_event.code == KeyCode::Char('?')
                    && !key_event.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.footer_mode = if matches!(self.footer_mode, FooterMode::ShortcutOverlay) {
                FooterMode::ShortcutSummary
            } else {
                FooterMode::ShortcutOverlay
            };
            return (InputResult::None, true);
        }
        if self.is_shell_mode
            && key_event.code == KeyCode::Tab
            && key_event.modifiers == KeyModifiers::NONE
            && key_event.kind == KeyEventKind::Press
        {
            self.app_event_tx.send(AppEvent::ShellComplete {
                text: self.textarea.text().to_owned(),
                cursor: self.textarea.cursor(),
            });
            return (InputResult::None, true);
        }
        if self.is_vim_operator_pending() {
            return self.handle_input_basic(key_event);
        }
        // `!` typed on an empty prompt starts a shell command. (With paste-burst detection on,
        // the burst's flush of a lone `!` does the same.)
        if self.disable_paste_burst
            && self.is_empty()
            && self.textarea.mode() == EditorMode::Insert
            && key_event.code == KeyCode::Char('!')
            && key_event.kind == KeyEventKind::Press
            && key_event.modifiers == KeyModifiers::NONE
        {
            self.is_shell_mode = true;
            return (InputResult::None, true);
        }
        if self.is_empty()
            && self.textarea.is_normal_mode()
            && key_event.kind == KeyEventKind::Press
            && key_event.modifiers == KeyModifiers::NONE
        {
            match key_event.code {
                KeyCode::Char('/') => {
                    self.textarea.enter_insert();
                    self.textarea.insert_str("/");
                    return (InputResult::None, true);
                }
                KeyCode::Char('!') => {
                    self.textarea.enter_insert();
                    self.is_shell_mode = true;
                    return (InputResult::None, true);
                }
                _ => {}
            }
        }
        if key_event.code != KeyCode::Esc {
            self.footer_mode = reset_mode_after_activity(self.footer_mode);
        }
        match key_event {
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: crossterm::event::KeyModifiers::CONTROL,
                kind: KeyEventKind::Press,
                ..
            } if self.is_empty() => {
                // Route through the widget's quit flow so Ctrl+D gets the
                // same detach feedback, input freeze, and hard-exit watchdog
                // as /quit — not an abrupt silent exit.
                self.app_event_tx.send(AppEvent::BeginExit);
                (InputResult::None, true)
            }
            // Next and previous model (Ctrl+N / Ctrl+P by default). With a popup open they move
            // through its list instead; the popup handles them before this point.
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::NextModel),
                        &key_event,
                    ) =>
            {
                self.app_event_tx
                    .send(AppEvent::CycleModel(crate::chatwidget::ModelStep::Next));
                (InputResult::None, true)
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::PreviousModel),
                        &key_event,
                    ) =>
            {
                self.app_event_tx
                    .send(AppEvent::CycleModel(crate::chatwidget::ModelStep::Previous));
                (InputResult::None, true)
            }
            // Next and previous effort level (Ctrl+Shift+N / Ctrl+Shift+P by default).
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::NextEffort),
                        &key_event,
                    ) =>
            {
                self.app_event_tx
                    .send(AppEvent::CycleEffort(crate::chatwidget::ModelStep::Next));
                (InputResult::None, true)
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::PreviousEffort),
                        &key_event,
                    ) =>
            {
                self.app_event_tx.send(AppEvent::CycleEffort(
                    crate::chatwidget::ModelStep::Previous,
                ));
                (InputResult::None, true)
            }
            // Next and previous model mode (Alt+. / Alt+, by default).
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::NextModelMode),
                        &key_event,
                    ) =>
            {
                self.app_event_tx
                    .send(AppEvent::CycleModelMode(crate::chatwidget::ModelStep::Next));
                (InputResult::None, true)
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::PreviousModelMode),
                        &key_event,
                    ) =>
            {
                self.app_event_tx.send(AppEvent::CycleModelMode(
                    crate::chatwidget::ModelStep::Previous,
                ));
                (InputResult::None, true)
            }
            // Older effort bindings, kept for terminals and habits that still send Shift.
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(&NEXT_EFFORT_ALIAS, &key_event) =>
            {
                self.app_event_tx
                    .send(AppEvent::CycleEffort(crate::chatwidget::ModelStep::Next));
                (InputResult::None, true)
            }
            key_event
                if key_event.kind == KeyEventKind::Press
                    && matches_binding(&PREVIOUS_EFFORT_ALIAS, &key_event) =>
            {
                self.app_event_tx.send(AppEvent::CycleEffort(
                    crate::chatwidget::ModelStep::Previous,
                ));
                (InputResult::None, true)
            }
            // `↑`/`↓` past the edges of the text walk the conversation's prompts and the history
            // (see `prompt_walk`); inside the text they are the editor's.
            key_event
                if key_event.kind == KeyEventKind::Press
                    && !self.textarea.is_normal_mode()
                    && matches_binding(
                        self.textarea
                            .hotkey_config()
                            .binding_for(nori_config::HotkeyAction::HistorySearch),
                        &key_event,
                    ) =>
            {
                self.active_popup = ActivePopup::HistorySearch(HistorySearchPopup::new());
                self.app_event_tx.send(AppEvent::HarnessAction(
                    crate::app_event::HarnessAction::SearchHistory { max_results: 500 },
                ));
                (InputResult::None, true)
            }
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // Vim's Enter behaviour: route by mode and the configured behaviour.
                if self.textarea.keymap() == KeymapKind::Vim {
                    let insert = self.textarea.mode() == EditorMode::Insert;
                    match (self.vim_enter_behavior, insert) {
                        // "Enter is Newline": Insert mode Enter inserts a newline.
                        (VimEnterBehavior::Newline, true) => {
                            self.textarea.insert_str("\n");
                            return (InputResult::None, true);
                        }
                        // "Enter is Submit": Normal mode Enter inserts a newline.
                        (VimEnterBehavior::Submit, false) => {
                            self.textarea.insert_str("\n");
                            return (InputResult::None, true);
                        }
                        (VimEnterBehavior::Newline, false)
                        | (VimEnterBehavior::Submit, true)
                        | (VimEnterBehavior::AlwaysSubmit, _)
                        | (VimEnterBehavior::Off, _) => {} // fall through to submit
                    }
                }

                if self.is_shell_mode {
                    let body = self.expand_pending_pastes(self.textarea.text().to_string());
                    self.textarea.set_text("");
                    self.is_shell_mode = false;
                    if body.trim().is_empty() {
                        return (InputResult::None, true);
                    }
                    let text = format!("!{body}");
                    return (InputResult::Submitted(text), true);
                }

                // If the first line is a bare built-in slash command (no args),
                // dispatch it even when the slash popup isn't visible. This preserves
                // the workflow: type a prefix ("/di"), press Tab to complete to
                // "/diff ", then press Enter to run it. Tab moves the cursor beyond
                // the '/name' token and our caret-based heuristic hides the popup,
                // but Enter should still dispatch the command rather than submit
                // literal text.
                let first_line = self.textarea.text().lines().next().unwrap_or("");
                if let Some((name, rest)) = parse_slash_name(first_line)
                    && rest.is_empty()
                    && let Some((_n, cmd)) = built_in_slash_commands()
                        .into_iter()
                        .find(|(n, _)| *n == name)
                {
                    self.textarea.set_text("");
                    return (InputResult::Command(cmd), true);
                }
                // A one-line `/name value` that names an action (`/model opus`) runs it.
                if !self.textarea.text().contains('\n')
                    && let Some(invocation) = self.actions.parse_command(first_line)
                    && crate::actions::builtin(&invocation.id).is_some()
                {
                    self.textarea.set_text("");
                    return (InputResult::Action(invocation), true);
                }
                // If we're in a paste-like burst capture, treat Enter as part of the burst
                // and accumulate it rather than submitting or inserting immediately.
                // Do not treat Enter as paste inside a slash-command context.
                let in_slash_context = matches!(self.active_popup, ActivePopup::Command(_))
                    || self
                        .textarea
                        .text()
                        .lines()
                        .next()
                        .unwrap_or("")
                        .starts_with('/');
                if self.paste_burst.is_active() && !in_slash_context {
                    let now = Instant::now();
                    if self.paste_burst.append_newline_if_active(now) {
                        return (InputResult::None, true);
                    }
                }
                // Images go with the prompt only while their chip is in it (a chip deleted and
                // brought back by undo brings its image back too).
                let raw_text = self.textarea.text().to_owned();
                self.attached_images
                    .retain(|image| raw_text.contains(image.chip.as_char()));
                // Chips become what they stand for (pastes their text, images their label);
                // directives are set aside to apply when the prompt is sent.
                if self
                    .textarea
                    .text()
                    .chars()
                    .any(|ch| ChipId::from_char(ch).is_some())
                {
                    let raw = self.textarea.text().to_string();
                    self.collect_directives(&raw);
                    let text = self.expand_pending_pastes(raw);
                    self.textarea.set_text(&text);
                }

                // During a paste-like burst, treat Enter as a newline instead of submit.
                let now = Instant::now();
                if !self.disable_paste_burst
                    && self
                        .paste_burst
                        .newline_should_insert_instead_of_submit(now)
                    && !in_slash_context
                {
                    self.textarea.insert_str("\n");
                    self.paste_burst.extend_window(now);
                    return (InputResult::None, true);
                }
                let mut text = self.textarea.text().to_string();
                let original_input = text.clone();
                let input_starts_with_space = original_input.starts_with(' ');
                self.textarea.set_text("");

                // If there is neither text nor attachments, suppress submission entirely.
                let has_attachments = !self.attached_images.is_empty();
                text = text.trim().to_string();
                // Alias a bare `exit`/`quit` to the matching slash command so the
                // user can quit without the leading slash. A leading space opts out
                // (mirroring how a leading space escapes slash commands below),
                // keeping the words available as literal messages to the agent.
                if !input_starts_with_space {
                    if text == "exit" {
                        return (InputResult::Command(SlashCommand::Exit), true);
                    }
                    if text == "quit" {
                        return (InputResult::Command(SlashCommand::Quit), true);
                    }
                }
                if let Some((name, _rest)) = parse_slash_name(&text) {
                    let treat_as_plain_text = input_starts_with_space || name.contains('/');
                    if !treat_as_plain_text {
                        let is_builtin = built_in_slash_commands()
                            .into_iter()
                            .any(|(command_name, _)| command_name == name);
                        let prompt_prefix = format!("{PROMPTS_CMD_PREFIX}:");
                        let is_known_prompt = name
                            .strip_prefix(&prompt_prefix)
                            .map(|prompt_name| {
                                self.custom_prompts
                                    .iter()
                                    .any(|prompt| prompt.name == prompt_name)
                            })
                            .unwrap_or(false);
                        let agent_prefix = if self.agent_command_prefix.is_empty() {
                            String::new()
                        } else {
                            format!("{}:", self.agent_command_prefix)
                        };
                        let is_agent_command = !agent_prefix.is_empty()
                            && name.strip_prefix(&agent_prefix).is_some_and(|cmd_name| {
                                self.agent_commands.iter().any(|cmd| cmd.name == cmd_name)
                            });
                        if !is_builtin && !is_known_prompt && !is_agent_command {
                            let message = format!(
                                r#"Unrecognized command '/{name}'. Type "/" for a list of supported commands."#
                            );
                            self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                                history_cell::new_info_event(message, None),
                            )));
                            self.textarea.set_text(&original_input);
                            self.textarea.set_cursor(original_input.len());
                            return (InputResult::None, true);
                        }
                    }
                }

                // Strip the agent command prefix so the ACP agent receives the
                // bare command name (e.g. "/loop 5m" instead of "/claude-code:loop 5m").
                if !self.agent_command_prefix.is_empty() {
                    let slash_prefix = format!("/{}:", self.agent_command_prefix);
                    if let Some(rest) = text.strip_prefix(&slash_prefix) {
                        text = format!("/{rest}");
                    }
                }

                // Intercept script-kind prompts before attempting expand_custom_prompt,
                // since scripts have empty content and would just submit empty text.
                if let Some((name, _rest)) = parse_slash_name(&text)
                    && let Some(prompt_name) = name.strip_prefix(&format!("{PROMPTS_CMD_PREFIX}:"))
                    && let Some(prompt) = self.custom_prompts.iter().find(|p| p.name == prompt_name)
                    && matches!(prompt.kind, CustomPromptKind::Script { .. })
                {
                    let args =
                        extract_positional_args_for_prompt_line(&original_input, &prompt.name);
                    self.app_event_tx.send(AppEvent::ExecuteScript {
                        prompt: prompt.clone(),
                        args,
                    });
                    self.textarea.set_text("");
                    return (InputResult::None, true);
                }

                let expanded_prompt = match expand_custom_prompt(&text, &self.custom_prompts) {
                    Ok(expanded) => expanded,
                    Err(err) => {
                        self.app_event_tx.send(AppEvent::InsertHistoryCell(Box::new(
                            history_cell::new_error_event(err.user_message()),
                        )));
                        self.textarea.set_text(&original_input);
                        self.textarea.set_cursor(original_input.len());
                        return (InputResult::None, true);
                    }
                };
                if let Some(expanded) = expanded_prompt {
                    text = expanded;
                }
                if text.is_empty() && !has_attachments {
                    return (InputResult::None, true);
                }
                // Do not clear attached_images here; ChatWidget drains them via take_recent_submission_images().
                (InputResult::Submitted(text), true)
            }
            input => self.handle_input_basic(input),
        }
    }

    /// Handle generic Input events that modify the textarea content.
    pub(super) fn handle_input_basic(&mut self, input: KeyEvent) -> (InputResult, bool) {
        // If we have a buffered non-bracketed paste burst and enough time has
        // elapsed since the last char, flush it before handling a new input.
        let now = Instant::now();
        self.handle_paste_burst_flush(now);

        if !matches!(input.code, KeyCode::Esc) {
            self.footer_mode = reset_mode_after_activity(self.footer_mode);
        }

        // Outside Insert mode keys are commands, not typing: no paste-burst detection.
        if self.textarea.mode() != EditorMode::Insert {
            self.editor_input(input);
            return (InputResult::None, true);
        }

        // If we're capturing a burst and receive Enter, accumulate it instead of inserting.
        if matches!(input.code, KeyCode::Enter)
            && self.paste_burst.is_active()
            && self.paste_burst.append_newline_if_active(now)
        {
            return (InputResult::None, true);
        }

        // Intercept plain Char inputs to optionally accumulate into a burst buffer (only where
        // pastes are not bracketed: then fast typing followed by Enter always submits).
        if !self.disable_paste_burst
            && let KeyEvent {
                code: KeyCode::Char(ch),
                modifiers,
                ..
            } = input
        {
            let has_ctrl_or_alt = has_ctrl_or_alt(modifiers);
            if !has_ctrl_or_alt {
                // Non-ASCII characters (e.g., from IMEs) can arrive in quick bursts and be
                // misclassified by paste heuristics. Flush any active burst buffer and insert
                // non-ASCII characters directly.
                if !ch.is_ascii() {
                    return self.handle_non_ascii_char(input);
                }

                match self.paste_burst.on_plain_char(ch, now) {
                    CharDecision::BufferAppend => {
                        self.paste_burst.append_char_to_buffer(ch, now);
                        return (InputResult::None, true);
                    }
                    CharDecision::BeginBuffer { retro_chars } => {
                        let cur = self.textarea.cursor();
                        let txt = self.textarea.text();
                        let safe_cur = Self::clamp_to_char_boundary(txt, cur);
                        let before = &txt[..safe_cur];
                        if let Some(grab) =
                            self.paste_burst
                                .decide_begin_buffer(now, before, retro_chars as usize)
                        {
                            if !grab.grabbed.is_empty() {
                                self.textarea.replace_range(grab.start_byte..safe_cur, "");
                            }
                            self.paste_burst.begin_with_retro_grabbed(grab.grabbed, now);
                            self.paste_burst.append_char_to_buffer(ch, now);
                            return (InputResult::None, true);
                        }
                        // If decide_begin_buffer opted not to start buffering,
                        // fall through to normal insertion below.
                    }
                    CharDecision::BeginBufferFromPending => {
                        // First char was held; now append the current one.
                        self.paste_burst.append_char_to_buffer(ch, now);
                        return (InputResult::None, true);
                    }
                    CharDecision::RetainFirstChar => {
                        // Keep the first fast char pending momentarily.
                        return (InputResult::None, true);
                    }
                }
            }
            if let Some(pasted) = self.paste_burst.flush_before_modified_input() {
                self.handle_paste_with_shell_detection(pasted, false);
            }
        }

        // Normal input handling
        self.editor_input(input);

        // Update paste-burst heuristic for plain Char (no Ctrl/Alt) events.
        let crossterm::event::KeyEvent {
            code, modifiers, ..
        } = input;
        match code {
            KeyCode::Char(_) => {
                let has_ctrl_or_alt = has_ctrl_or_alt(modifiers);
                if has_ctrl_or_alt {
                    self.paste_burst.clear_window_after_non_char();
                }
            }
            KeyCode::Enter => {
                // Keep burst window alive (supports blank lines in paste).
            }
            _ => {
                // Other keys: clear burst window (buffer should have been flushed above if needed).
                self.paste_burst.clear_window_after_non_char();
            }
        }

        (InputResult::None, true)
    }

    /// The text as sent: each paste chip becomes what was pasted, every other chip its label.
    pub(super) fn expand_pending_pastes(&mut self, text: String) -> String {
        if !text.chars().any(|ch| ChipId::from_char(ch).is_some()) {
            self.pending_pastes.clear();
            return text;
        }
        let mut expanded = String::with_capacity(text.len());
        for ch in text.chars() {
            let Some(id) = ChipId::from_char(ch) else {
                expanded.push(ch);
                continue;
            };
            if self.is_directive(id) {
                // Directives are applied, not sent.
                continue;
            }
            match self.pending_pastes.iter().find(|(chip, _)| *chip == id) {
                Some((_, pasted)) => expanded.push_str(pasted),
                None => expanded.push_str(self.textarea.chip_label(id).unwrap_or_default()),
            }
        }
        self.pending_pastes.clear();
        expanded
    }
}
