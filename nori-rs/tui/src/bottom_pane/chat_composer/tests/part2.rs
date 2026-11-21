use super::*;
use pretty_assertions::assert_eq;

#[test]
fn deleting_a_paste_chip_drops_the_paste_from_what_is_sent_and_undo_restores_it() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let large = "y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    composer.handle_paste(large.clone());
    composer.insert_str(" tail");
    let chip = composer.pending_pastes[0].0;
    composer.textarea.set_cursor(chip.as_char().len_utf8());

    composer.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(composer.textarea.text(), " tail");

    // The payload stays until the prompt is sent, so that undoing brings the paste back whole.
    composer.handle_key_event(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
    assert_eq!(composer.textarea.text(), format!("{} tail", chip.as_char()));
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted(format!("{large} tail")));
    assert_eq!(composer.pending_pastes, vec![]);
}

#[test]
fn ui_snapshots() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut terminal = match Terminal::new(TestBackend::new(100, 10)) {
        Ok(t) => t,
        Err(e) => panic!("Failed to create terminal: {e}"),
    };

    let test_cases = vec![
        ("empty", None),
        ("small", Some("short".to_string())),
        ("large", Some("z".repeat(LARGE_PASTE_CHAR_THRESHOLD + 5))),
        ("multiple_pastes", None),
        ("backspace_after_pastes", None),
    ];

    for (name, input) in test_cases {
        // Create a fresh composer for each test case
        let mut composer = ChatComposer::new(
            true,
            sender.clone(),
            false,
            "Ask Nori to do anything".to_string(),
            false,
        );

        if let Some(text) = input {
            composer.handle_paste(text);
        } else if name == "multiple_pastes" {
            // First large paste
            composer.handle_paste("x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 3));
            // Second large paste
            composer.handle_paste("y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 7));
            // Small paste
            composer.handle_paste(" another short paste".to_string());
        } else if name == "backspace_after_pastes" {
            // Three large pastes
            composer.handle_paste("a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 2));
            composer.handle_paste("b".repeat(LARGE_PASTE_CHAR_THRESHOLD + 4));
            composer.handle_paste("c".repeat(LARGE_PASTE_CHAR_THRESHOLD + 6));
            // Move cursor to end and press backspace
            composer.textarea.set_cursor(composer.textarea.text().len());
            composer.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
        }

        terminal
            .draw(|f| composer.render(f.area(), f.buffer_mut()))
            .unwrap_or_else(|e| panic!("Failed to draw {name} composer: {e}"));

        insta::assert_snapshot!(name, terminal.backend());
    }
}

#[test]
fn slash_popup_model_first_for_mo_ui() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);

    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    // Type "/mo" humanlike so paste-burst doesn’t interfere.
    type_chars_humanlike(&mut composer, &['/', 'm', 'o']);

    let mut terminal = match Terminal::new(TestBackend::new(60, 5)) {
        Ok(t) => t,
        Err(e) => panic!("Failed to create terminal: {e}"),
    };
    terminal
        .draw(|f| composer.render(f.area(), f.buffer_mut()))
        .unwrap_or_else(|e| panic!("Failed to draw composer: {e}"));

    // Visual snapshot should show the slash popup with /model as the first entry.
    insta::assert_snapshot!("slash_popup_mo", terminal.backend());
}

#[test]
fn slash_popup_close_visible_for_cl_ui() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);

    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    // Type "/cl" humanlike so paste-burst doesn’t interfere.
    type_chars_humanlike(&mut composer, &['/', 'c', 'l']);

    let mut terminal = match Terminal::new(TestBackend::new(60, 5)) {
        Ok(t) => t,
        Err(e) => panic!("Failed to create terminal: {e}"),
    };
    terminal
        .draw(|f| composer.render(f.area(), f.buffer_mut()))
        .unwrap_or_else(|e| panic!("Failed to draw composer: {e}"));

    // Visual snapshot should show the slash popup offering /close.
    insta::assert_snapshot!("slash_popup_cl", terminal.backend());
}

#[test]
fn slash_popup_model_first_for_mo_logic() {
    use crate::bottom_pane::command_popup::CommandItem;
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    type_chars_humanlike(&mut composer, &['/', 'm', 'o']);

    match &composer.active_popup {
        ActivePopup::Command(popup) => match popup.selected_item() {
            Some(CommandItem::Builtin(cmd)) => {
                assert_eq!(cmd.command(), "model")
            }
            Some(CommandItem::UserPrompt(_) | CommandItem::AgentCommand(_)) => {
                panic!("unexpected non-builtin selected for '/mo'")
            }
            None => panic!("no selected command for '/mo'"),
        },
        _ => panic!("slash popup not active after typing '/mo'"),
    }
}

#[test]
fn composer_status_line_does_not_carry_the_acp_mode_label() {
    snapshot_composer_state("composer_acp_mode_not_in_status_line", false, |composer| {
        composer.set_acp_mode_label(Some("Plan".to_string()));
    });
}

#[test]
fn composer_can_place_the_mode_segment_in_a_prompt_corner() {
    snapshot_composer_state("composer_acp_mode_prompt_corner", false, |composer| {
        composer.set_footer_layout_config(nori_config::FooterLayoutConfig::from_toml(
            &nori_config::FooterLayoutConfigToml {
                textarea_top_right: Some(vec![nori_config::FooterSegment::ModeIndicator.into()]),
                ..Default::default()
            },
        ));
        composer.set_acp_mode_label(Some("Plan".to_string()));
    });
}

/// Render the composer into an exact `width` x `height` area and return the
/// rows as plain text, so narrow-terminal tests can inspect every cell.
fn render_composer_rows(width: u16, height: u16, acp_mode_label: &str) -> Vec<String> {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    let layout_toml: nori_config::NoriConfigToml = toml::from_str(
        r#"[tui.footer_layout]
textarea_top_right = ["mode_indicator"]"#,
    )
    .unwrap();
    composer.set_footer_layout_config(nori_config::FooterLayoutConfig::from_toml(
        &layout_toml.tui.footer_layout,
    ));
    composer.set_acp_mode_label(Some(acp_mode_label.to_string()));

    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| composer.render(f.area(), f.buffer_mut()))
        .unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

#[test]
fn textarea_corner_clips_without_overwriting_the_prompt() {
    // Comfortably wide: the mode indicator sits above the prompt.
    let wide = render_composer_rows(40, 3, "Plan");
    assert!(wide[0].trim_end().ends_with("[ Plan ]"), "{wide:?}");
    assert!(wide[1].starts_with("› Ask Nori"), "{wide:?}");

    // A narrow composer clips the indicator to its own padding row.
    let narrow = render_composer_rows(6, 3, "Plan");
    assert_eq!(narrow[0].trim(), "[ Pla", "{narrow:?}");
    assert!(narrow[1].starts_with('›'), "{narrow:?}");
}

#[test]
fn textarea_corner_never_lands_on_the_prompt_row() {
    // A composer squeezed below its three-row minimum has no padding row for
    // the corner segment, so nothing is drawn over the prompt.
    for height in [1, 2] {
        let rows = render_composer_rows(40, height, "Plan");
        for (index, row) in rows.iter().enumerate() {
            assert!(
                !row.contains("[ Plan ]"),
                "corner segment landed on row {index} of a {height}-row composer: {rows:?}"
            );
        }
    }
}

#[test]
fn slash_init_dispatches_command_and_does_not_submit_literal_text() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    // Type the slash command.
    type_chars_humanlike(&mut composer, &['/', 'i', 'n', 'i', 't']);

    // Press Enter to dispatch the selected command.
    let (result, _needs_redraw) =
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    // When a slash command is dispatched, the composer should return a
    // Command result (not submit literal text) and clear its textarea.
    match result {
        InputResult::Command(cmd) => {
            assert_eq!(cmd.command(), "init");
        }
        InputResult::Submitted(text) => {
            panic!("expected command dispatch, but composer submitted literal text: {text}")
        }
        InputResult::None
        | InputResult::Steer(_)
        | InputResult::PromoteQueued
        | InputResult::Action(_) => {
            panic!("expected Command result for '/init'")
        }
    }
    assert!(composer.textarea.is_empty(), "composer should be cleared");
}

#[test]
fn extract_args_supports_quoted_paths_single_arg() {
    let args =
        extract_positional_args_for_prompt_line("/prompts:review \"docs/My File.md\"", "review");
    assert_eq!(args, vec!["docs/My File.md".to_string()]);
}

#[test]
fn extract_args_supports_mixed_quoted_and_unquoted() {
    let args =
        extract_positional_args_for_prompt_line("/prompts:cmd \"with spaces\" simple", "cmd");
    assert_eq!(args, vec!["with spaces".to_string(), "simple".to_string()]);
}

#[test]
fn slash_tab_completion_moves_cursor_to_end() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    // Use /di to match /diff, not /compact
    type_chars_humanlike(&mut composer, &['/', 'd', 'i']);

    let (_result, _needs_redraw) =
        composer.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert_eq!(composer.textarea.text(), "/diff ");
    assert_eq!(composer.textarea.cursor(), composer.textarea.text().len());
}

#[test]
fn slash_tab_then_enter_dispatches_builtin_command() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    // Type a prefix and complete with Tab, which inserts a trailing space
    // and moves the cursor beyond the '/name' token (hides the popup).
    type_chars_humanlike(&mut composer, &['/', 'd', 'i']);
    let (_res, _redraw) =
        composer.handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(composer.textarea.text(), "/diff ");

    // Press Enter: should dispatch the command, not submit literal text.
    let (result, _needs_redraw) =
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match result {
        InputResult::Command(cmd) => assert_eq!(cmd.command(), "diff"),
        InputResult::Submitted(text) => {
            panic!("expected command dispatch after Tab completion, got literal submit: {text}")
        }
        InputResult::None
        | InputResult::Steer(_)
        | InputResult::PromoteQueued
        | InputResult::Action(_) => {
            panic!("expected Command result for '/diff'")
        }
    }
    assert!(composer.textarea.is_empty());
}

#[test]
fn slash_mention_dispatches_command_and_inserts_at() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    type_chars_humanlike(&mut composer, &['/', 'm', 'e', 'n', 't', 'i', 'o', 'n']);

    let (result, _needs_redraw) =
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    match result {
        InputResult::Command(cmd) => {
            assert_eq!(cmd.command(), "mention");
        }
        InputResult::Submitted(text) => {
            panic!("expected command dispatch, but composer submitted literal text: {text}")
        }
        InputResult::None
        | InputResult::Steer(_)
        | InputResult::PromoteQueued
        | InputResult::Action(_) => {
            panic!("expected Command result for '/mention'")
        }
    }
    assert!(composer.textarea.is_empty(), "composer should be cleared");
    composer.insert_str("@");
    assert_eq!(composer.textarea.text(), "@");
}

#[test]
fn test_multiple_pastes_submission() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    let first = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 3);
    let second = "y".repeat(LARGE_PASTE_CHAR_THRESHOLD + 7);

    composer.handle_paste(first.clone());
    composer.handle_paste(" and ".to_string());
    composer.handle_paste(second.clone());

    // Each large paste is one chip character; the small one is plain text.
    let chips: Vec<_> = composer
        .pending_pastes
        .iter()
        .map(|(chip, _)| chip.as_char())
        .collect();
    assert_eq!(chips.len(), 2);
    assert_eq!(
        composer.textarea.text(),
        format!("{} and {}", chips[0], chips[1])
    );

    // Submit and verify final expansion
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    if let InputResult::Submitted(text) = result {
        assert_eq!(text, format!("{first} and {second}"));
    } else {
        panic!("expected Submitted");
    }
}

#[test]
fn backspace_deletes_a_paste_chip_as_one_character() {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;

    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.handle_paste("a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 5));
    composer.handle_paste(" and ".to_string());
    composer.handle_paste("b".repeat(LARGE_PASTE_CHAR_THRESHOLD + 6));
    let chips: Vec<char> = composer
        .pending_pastes
        .iter()
        .map(|(chip, _)| chip.as_char())
        .collect();
    assert_eq!(
        composer.textarea.text(),
        format!("{} and {}", chips[0], chips[1])
    );

    // Backspace right after the first chip deletes it as one character.
    composer.textarea.set_cursor(chips[0].len_utf8());
    composer.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(composer.textarea.text(), format!(" and {}", chips[1]));

    // Backspace at the end deletes the second one.
    composer.textarea.set_cursor(composer.textarea.text().len());
    composer.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(composer.textarea.text(), " and ");
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted("and".to_string()));
}

#[test]
fn delete_key_removes_the_chip_after_the_cursor() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.handle_paste("x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 4));
    composer.insert_str("tail");
    composer.textarea.set_cursor(0);
    composer.handle_key_event(KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE));
    assert_eq!(composer.textarea.text(), "tail");
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted("tail".to_string()));
}

#[test]
fn deleting_one_equal_length_paste_keeps_the_other_payload() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    let first = "a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 10);
    let second = "b".repeat(LARGE_PASTE_CHAR_THRESHOLD + 10);

    composer.handle_paste(first);
    composer.handle_paste(second.clone());
    let remaining = composer.pending_pastes[1].0.as_char();
    let first_chip_len = composer.pending_pastes[0].0.as_char().len_utf8();
    composer.textarea.set_cursor(first_chip_len);
    composer.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));

    assert_eq!(composer.current_text(), remaining.to_string());
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted(second));
}

#[test]
fn prompt_bar_metadata_stays_inside_the_composer() {
    snapshot_composer_state("prompt_bar_git_context_model", false, |composer| {
        composer.context_window_percent = Some(44);
        composer.set_model_label(Some("GPT-6 Astra".to_string()));
        composer.set_acp_mode_label(Some("Plan".to_string()));
        let layout = toml::from_str::<nori_config::FooterLayoutConfigToml>(
            r#"textarea_top_left = [{ format = "⎇ main · {context}" }]"#,
        )
        .unwrap();
        composer.set_footer_layout_config(nori_config::FooterLayoutConfig::from_toml(&layout));
    });
}

#[test]
fn prompt_bar_corners_never_overwrite_each_other() {
    let rect = Rect::new(0, 0, 24, 3);
    let mut buffer = Buffer::empty(rect);
    render_textarea_corner_segments(
        rect,
        &mut buffer,
        crate::bottom_pane::footer::TextareaCornerSegments {
            top_left: "⎇ long-branch · 44%".into(),
            top_right: "[ Plan ]".into(),
            ..Default::default()
        },
    );
    let row: String = (0..rect.width).map(|x| buffer[(x, 0)].symbol()).collect();
    insta::assert_snapshot!("prompt_bar_narrow_corners", row);
}

#[test]
fn long_branch_keeps_insert_indicator_and_model_visible() {
    let rect = Rect::new(0, 0, 42, 3);
    let mut buffer = Buffer::empty(rect);
    render_textarea_corner_segments(
        rect,
        &mut buffer,
        crate::bottom_pane::footer::TextareaCornerSegments {
            top_left: "INSERT · git very-long-feature-branch · 44%".into(),
            top_right: "OpenCode Big Pickle".into(),
            ..Default::default()
        },
    );
    let row: String = (0..rect.width).map(|x| buffer[(x, 0)].symbol()).collect();
    assert!(row.contains("INSERT"), "{row:?}");
    assert!(row.contains("git"), "{row:?}");
    assert!(row.contains("OpenCode Big Pickle"), "{row:?}");
}
