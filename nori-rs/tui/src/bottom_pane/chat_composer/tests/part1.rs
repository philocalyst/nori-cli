use super::*;
use crate::app_event::RewindFiles;
use crate::app_event::RewindSelection;
use pretty_assertions::assert_eq;

#[test]
fn shortcut_hint_is_placeholder_only_until_shortcut_overlay_opens() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let composer = ChatComposer::new(true, sender, false, "? for shortcuts".to_string(), false);

    let area = Rect::new(0, 0, 40, 6);
    let mut buf = Buffer::empty(area);
    composer.render(area, &mut buf);

    let row_to_string = |y: u16| {
        let mut row = String::new();
        for x in 0..area.width {
            row.push(buf[(x, y)].symbol().chars().next().unwrap_or(' '));
        }
        row
    };

    let input_row = row_to_string(1);
    assert!(
        input_row.contains("? for shortcuts"),
        "expected shortcut hint in the empty composer placeholder: {input_row:?}",
    );

    let footer_rows = (3..area.height).map(row_to_string).collect::<Vec<_>>();
    assert_eq!(
        footer_rows
            .iter()
            .filter(|row| row.contains("? for shortcuts"))
            .count(),
        0,
        "shortcut hint should not render below the textarea: {footer_rows:?}",
    );
}

#[test]
fn question_mark_on_an_empty_prompt_opens_and_closes_the_key_list() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    type_chars_humanlike(&mut composer, &['?']);
    assert_eq!(composer.current_text(), "");
    assert_eq!(composer.footer_mode(), FooterMode::ShortcutOverlay);

    type_chars_humanlike(&mut composer, &['?']);
    assert_eq!(composer.current_text(), "");
    assert_ne!(composer.footer_mode(), FooterMode::ShortcutOverlay);
}

#[test]
fn clear_for_ctrl_c_records_cleared_draft() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    composer.set_text_content("draft text".to_string());
    assert_eq!(composer.clear_for_ctrl_c(), Some("draft text".to_string()));
    assert!(composer.is_empty());

    // `↓` on the empty composer brings the cleared draft back.
    composer.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(composer.current_text(), "draft text");
}

#[test]
fn question_mark_within_a_draft_is_literal() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    type_chars_humanlike(&mut composer, &['h', '?', '?']);
    assert_eq!(composer.current_text(), "h??");
    assert_ne!(composer.footer_mode(), FooterMode::ShortcutOverlay);
}

#[test]
fn question_mark_lists_the_keys_while_a_task_is_running() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_task_running(true);
    type_chars_humanlike(&mut composer, &['?']);
    assert_eq!(composer.current_text(), "");
    assert_eq!(composer.footer_mode(), FooterMode::ShortcutOverlay);
}

#[test]
fn test_current_at_token_basic_cases() {
    let test_cases = vec![
        // Valid @ tokens
        ("@hello", 3, Some("hello".to_string()), "Basic ASCII token"),
        (
            "@file.txt",
            4,
            Some("file.txt".to_string()),
            "ASCII with extension",
        ),
        (
            "hello @world test",
            8,
            Some("world".to_string()),
            "ASCII token in middle",
        ),
        (
            "@test123",
            5,
            Some("test123".to_string()),
            "ASCII with numbers",
        ),
        // Unicode examples
        ("@İstanbul", 3, Some("İstanbul".to_string()), "Turkish text"),
        (
            "@testЙЦУ.rs",
            8,
            Some("testЙЦУ.rs".to_string()),
            "Mixed ASCII and Cyrillic",
        ),
        ("@诶", 2, Some("诶".to_string()), "Chinese character"),
        ("@👍", 2, Some("👍".to_string()), "Emoji token"),
        // Invalid cases (should return None)
        ("hello", 2, None, "No @ symbol"),
        (
            "@",
            1,
            Some("".to_string()),
            "Only @ symbol triggers empty query",
        ),
        ("@ hello", 2, None, "@ followed by space"),
        ("test @ world", 6, None, "@ with spaces around"),
    ];

    for (input, cursor_pos, expected, description) in test_cases {
        let mut textarea = TextArea::new();
        textarea.insert_str(input);
        textarea.set_cursor(cursor_pos);

        let result = ChatComposer::current_at_token(&textarea);
        assert_eq!(
            result, expected,
            "Failed for case: {description} - input: '{input}', cursor: {cursor_pos}"
        );
    }
}

#[test]
fn test_current_at_token_cursor_positions() {
    let test_cases = vec![
        // Different cursor positions within a token
        ("@test", 0, Some("test".to_string()), "Cursor at @"),
        ("@test", 1, Some("test".to_string()), "Cursor after @"),
        ("@test", 5, Some("test".to_string()), "Cursor at end"),
        // Multiple tokens - cursor determines which token
        ("@file1 @file2", 0, Some("file1".to_string()), "First token"),
        (
            "@file1 @file2",
            8,
            Some("file2".to_string()),
            "Second token",
        ),
        // Edge cases
        ("@", 0, Some("".to_string()), "Only @ symbol"),
        ("@a", 2, Some("a".to_string()), "Single character after @"),
        ("", 0, None, "Empty input"),
    ];

    for (input, cursor_pos, expected, description) in test_cases {
        let mut textarea = TextArea::new();
        textarea.insert_str(input);
        textarea.set_cursor(cursor_pos);

        let result = ChatComposer::current_at_token(&textarea);
        assert_eq!(
            result, expected,
            "Failed for cursor position case: {description} - input: '{input}', cursor: {cursor_pos}",
        );
    }
}

#[test]
fn test_current_at_token_whitespace_boundaries() {
    let test_cases = vec![
        // Space boundaries
        (
            "aaa@aaa",
            4,
            None,
            "Connected @ token - no completion by design",
        ),
        (
            "aaa @aaa",
            5,
            Some("aaa".to_string()),
            "@ token after space",
        ),
        (
            "test @file.txt",
            7,
            Some("file.txt".to_string()),
            "@ token after space",
        ),
        // Full-width space boundaries
        (
            "test　@İstanbul",
            8,
            Some("İstanbul".to_string()),
            "@ token after full-width space",
        ),
        (
            "@ЙЦУ　@诶",
            10,
            Some("诶".to_string()),
            "Full-width space between Unicode tokens",
        ),
        // Tab and newline boundaries
        (
            "test\t@file",
            6,
            Some("file".to_string()),
            "@ token after tab",
        ),
    ];

    for (input, cursor_pos, expected, description) in test_cases {
        let mut textarea = TextArea::new();
        textarea.insert_str(input);
        textarea.set_cursor(cursor_pos);

        let result = ChatComposer::current_at_token(&textarea);
        assert_eq!(
            result, expected,
            "Failed for whitespace boundary case: {description} - input: '{input}', cursor: {cursor_pos}",
        );
    }
}

#[test]
fn ascii_prefix_survives_non_ascii_followup() {
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

    let _ = composer.handle_key_event(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert!(composer.is_in_paste_burst());

    let _ = composer.handle_key_event(KeyEvent::new(KeyCode::Char('あ'), KeyModifiers::NONE));

    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match result {
        InputResult::Submitted(text) => assert_eq!(text, "1あ"),
        _ => panic!("expected Submitted"),
    }
}

#[test]
fn handle_paste_small_inserts_text() {
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

    let needs_redraw = composer.handle_paste("hello".to_string());
    assert!(needs_redraw);
    assert_eq!(composer.textarea.text(), "hello");
    assert!(composer.pending_pastes.is_empty());

    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match result {
        InputResult::Submitted(text) => assert_eq!(text, "hello"),
        _ => panic!("expected Submitted"),
    }
}

#[test]
fn empty_enter_returns_none() {
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

    // Ensure composer is empty and press Enter.
    assert!(composer.textarea.text().is_empty());
    let (result, _needs_redraw) =
        composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));

    match result {
        InputResult::None => {}
        other => panic!("expected None for empty enter, got: {other:?}"),
    }
}

#[test]
fn handle_paste_large_uses_placeholder_and_replaces_on_submit() {
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

    let large = "x".repeat(LARGE_PASTE_CHAR_THRESHOLD + 10);
    let needs_redraw = composer.handle_paste(large.clone());
    assert!(needs_redraw);
    let label = format!("[Pasted Content {} chars]", large.chars().count());
    let (chip, payload) = composer.pending_pastes[0].clone();
    assert_eq!(composer.textarea.text(), chip.as_char().to_string());
    assert_eq!(composer.textarea.chip_label(chip), Some(label.as_str()));
    assert_eq!(composer.pending_pastes, vec![(chip, large.clone())]);
    assert_eq!(payload, large);

    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    match result {
        InputResult::Submitted(text) => assert_eq!(text, large),
        _ => panic!("expected Submitted"),
    }
    assert!(composer.pending_pastes.is_empty());
}

#[test]
fn paste_normalizes_newlines_and_sanitizes_terminal_controls() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );

    composer.handle_paste("one\r\ntwo\rthree\t_count_r\x1b[13;2:3uows\n\0four\u{7f}".to_string());

    assert_eq!(
        composer.current_text(),
        "one\ntwo\nthree\t_count_rows\nfour"
    );
}

#[test]
fn equal_length_large_pastes_expand_in_order_on_submit() {
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

    composer.handle_paste(first.clone());
    composer.handle_paste(second.clone());

    // Two chips with the same label are still two distinct characters.
    let chips: Vec<char> = composer
        .pending_pastes
        .iter()
        .map(|(chip, _)| chip.as_char())
        .collect();
    assert_eq!(chips.len(), 2);
    assert_ne!(chips[0], chips[1]);
    assert_eq!(composer.current_text(), chips.iter().collect::<String>());
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted(format!("{first}{second}")));
}

#[test]
fn equal_length_large_pastes_expand_in_order_in_shell_mode() {
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

    composer.handle_paste(format!("!{first}"));
    composer.handle_paste(second.clone());

    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted(format!("!{first}{second}")));
}

fn prompt(cell_index: usize, text: &str) -> RecalledPrompt {
    RecalledPrompt {
        cell_index,
        text: text.to_string(),
    }
}

#[test]
fn prompt_walk_places_cursor_at_end_and_keeps_walking() {
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_conversation_prompts(
        1,
        (2, 0),
        vec![prompt(3, "older"), prompt(7, "newer\nentry")],
    );

    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(composer.current_text(), "newer\nentry");
    assert_eq!(composer.textarea.cursor(), "newer\nentry".len());
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::FocusChanged {
            generation: 1,
            cell: Some(7)
        })
    ));

    // The caret is on the last row: `↑` moves it within the prompt first.
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(composer.current_text(), "newer\nentry");
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(composer.current_text(), "older");
    assert_eq!(composer.textarea.cursor(), "older".len());
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::FocusChanged {
            generation: 1,
            cell: Some(3)
        })
    ));
}

#[test]
fn prompt_walk_enter_rewinds_to_the_edited_prompt_and_esc_restores_the_draft() {
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_conversation_prompts(1, (2, 0), vec![prompt(3, "first"), prompt(7, "second")]);
    composer.insert_str("my draft");

    // The caret is on the only row of the draft: `↑` starts the walk and stashes it.
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(composer.current_text(), "second");
    composer.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    // The modeless keymap has no use for `Esc`, so it ends the walk.
    assert_eq!(composer.current_text(), "my draft");

    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    composer.insert_str(" edited");
    while rx.try_recv().is_ok() {}
    let (result, _) = composer.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::None);
    assert!(matches!(
        rx.try_recv(),
        Ok(AppEvent::RewindSelection(RewindSelection {
            generation: 1,
            cell_index: 7,
            turn: 2,
            files: RewindFiles::Restore,
            ref text,
        })) if text == "second edited"
    ));
    assert_eq!(composer.current_text(), "my draft");
}

#[test]
fn asynchronous_history_recall_places_cursor_at_end() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_history_metadata(7, 1);
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));

    assert!(composer.on_history_entry_response(7, 0, Some("remote\nentry".to_string())));
    assert_eq!(composer.current_text(), "remote\nentry");
    assert_eq!(composer.textarea.cursor(), "remote\nentry".len());
}

/// A remote history response that lands after exit began must not touch the
/// frozen composer — neither its text nor its cursor.
#[test]
fn stale_history_response_while_exiting_leaves_composer_untouched() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let sender = AppEventSender::new(tx);
    let mut composer = ChatComposer::new(
        true,
        sender,
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_history_metadata(7, 1);
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    composer.insert_str("draft");
    composer.textarea.set_cursor(0);

    composer.show_exit_in_progress();
    composer.on_history_entry_response(7, 0, Some("remote entry".to_string()));

    assert_eq!(composer.current_text(), "draft");
    assert_eq!(composer.textarea.cursor(), 0);
}

#[test]
fn a_walked_prompt_is_edited_in_place_and_the_composer_keeps_only_its_status() {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_conversation_prompts(1, (2, 0), vec![prompt(3, "pears and apples")]);
    let before = composer.desired_height(60);
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    let edit = composer
        .inline_walk(60)
        .expect("the walked prompt is edited in place");
    let rows: Vec<String> = edit
        .body
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect();
    assert_eq!(
        (rows, edit.cursor),
        (vec!["pears and apples".to_owned()], Some((16, 0)))
    );
    // No text box under the conversation: the walk's line and the status line only.
    assert!(composer.desired_height(60) < before);
    assert_eq!(
        composer.cursor_pos(ratatui::layout::Rect::new(0, 0, 60, 10)),
        None
    );
}

#[test]
fn ctrl_f_on_a_walked_prompt_forks_a_subagent_from_its_turn() {
    let (tx, mut rx) = unbounded_channel::<AppEvent>();
    let mut composer = ChatComposer::new(
        true,
        AppEventSender::new(tx),
        false,
        "Ask Nori to do anything".to_string(),
        false,
    );
    composer.set_conversation_prompts(1, (2, 0), vec![prompt(3, "older"), prompt(7, "newer")]);
    composer.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    composer.handle_key_event(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL));
    let events: Vec<AppEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
    assert!(
        events.iter().any(|event| matches!(
            event,
            AppEvent::ForkWalkedTurn {
                generation: 1,
                cell_index: 7
            }
        )),
        "{events:?}"
    );
    // The walk is over and the draft is back.
    assert!(composer.walk_badge().is_none());
}

#[test]
fn the_walk_hint_drops_whole_keys_on_a_narrow_screen() {
    let text = |line: ratatui::text::Line<'static>| {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let badge = WalkBadge::Turn { turn: 2, total: 2 };
    assert_eq!(
        [80, 42, 32, 30, 18].map(|width| text(badge.hint(width))),
        [
            "↶ turn 2 of 2 · ⏎ rewind here · ⌥⏎ keep files · ^F fork · esc back".to_owned(),
            "↶ turn 2 of 2 · ⏎ rewind here · esc back".to_owned(),
            "↶ 2/2 · ⏎ rewind here · esc back".to_owned(),
            "↶ turn 2 of 2 · ⏎ rewind here".to_owned(),
            "↶ turn 2 of 2".to_owned(),
        ]
    );
}
