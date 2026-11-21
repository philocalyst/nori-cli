use super::*;
use pretty_assertions::assert_eq;

fn composer() -> ChatComposer {
    let (tx, _rx) = unbounded_channel::<AppEvent>();
    ChatComposer::new(true, AppEventSender::new(tx), false, String::new(), false)
}

#[test]
fn replacement_text_appends_typing_and_discards_old_buffered_input() {
    for replacement in ["draft", "héllo", "!echo hi", ""] {
        let mut composer = composer();
        composer.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        composer.set_text_content(replacement.to_string());
        composer.handle_paste_burst_flush(Instant::now() + Duration::from_secs(1));
        composer.insert_str(" suffix");
        assert_eq!(composer.current_text(), format!("{replacement} suffix"));
    }
}

#[test]
fn handoff_preserves_cursor_at_every_input_boundary() {
    let input = "draft message";
    for split in 0..=input.len() {
        let mut before = composer();
        before.handle_paste(input[..split].to_string());
        let mut after = composer();
        after.restore_draft(before.take_draft());
        after.handle_paste(input[split..].to_string());
        assert_eq!(after.current_text(), input, "handoff at {split}");
    }
}

#[test]
fn handoff_preserves_pending_first_character_and_paste_burst() {
    for input in ["d", "draft message"] {
        let mut before = composer();
        // Construct the burst at one logical instant, independent of scheduling.
        let now = Instant::now();
        for ch in input.chars() {
            match before.paste_burst.on_plain_char(ch, now) {
                CharDecision::RetainFirstChar => {}
                CharDecision::BeginBufferFromPending | CharDecision::BufferAppend => {
                    before.paste_burst.append_char_to_buffer(ch, now);
                }
                CharDecision::BeginBuffer { .. } => panic!("first character was held"),
            }
        }
        assert_eq!(before.current_text(), "");
        let mut after = composer();
        after.restore_draft(before.take_draft());
        after.handle_paste_burst_flush(now + Duration::from_secs(1));
        assert_eq!(after.current_text(), input);
        after.insert_str(" suffix");
        assert_eq!(after.current_text(), format!("{input} suffix"));
    }
}

#[test]
fn handoff_preserves_mid_text_cursor_and_undo() {
    let mut before = composer();
    before.handle_paste("héllo world".to_string());
    before.textarea.set_cursor("héllo".len());
    let mut after = composer();
    after.restore_draft(before.take_draft());
    after.insert_str(" brave");
    assert_eq!(after.current_text(), "héllo brave world");
    let ctrl_z = KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL);
    after.handle_key_event(ctrl_z);
    assert_eq!(after.current_text(), "héllo world");
    after.handle_key_event(ctrl_z);
    assert_eq!(after.current_text(), "");
}

#[test]
fn handoff_preserves_vim_normal_mode() {
    use nori_editor::EditorMode;

    let mut before = composer();
    before.set_vim_mode(nori_config::VimEnterBehavior::Submit);
    before.insert_str("draft");
    before.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let mut after = composer();
    after.set_vim_mode(nori_config::VimEnterBehavior::Submit);
    after.restore_draft(before.take_draft());
    assert_eq!(after.editor_mode(), EditorMode::Normal);
    after.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert_eq!(after.current_text(), "draf");
    after.handle_key_event(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
    assert_eq!(after.current_text(), "draft");
}

#[test]
fn handoff_preserves_shell_mode() {
    let mut before = composer();
    before.handle_paste("!echo".to_string());
    let mut after = composer();
    after.restore_draft(before.take_draft());
    after.handle_paste(" hello".to_string());
    assert_eq!(after.current_text(), "!echo hello");
    let (result, _) = after.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(result, InputResult::Submitted("!echo hello".to_string()));
}

#[test]
fn handoff_preserves_large_paste_payload_and_attached_image() {
    let mut before = composer();
    let payload = "a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1);
    let image = PathBuf::from("/tmp/draft.png");
    before.handle_paste(payload.clone());
    before.attach_image(image.clone(), 32, 16, "PNG");
    let mut after = composer();
    after.restore_draft(before.take_draft());
    let (result, _) = after.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        result,
        InputResult::Submitted(format!("{payload}[draft.png 32x16]"))
    );
    assert_eq!(after.take_recent_submission_images(), vec![image]);
}

#[test]
fn handoff_preserves_atomic_chip_editing() {
    let mut before = composer();
    before.handle_paste("a".repeat(LARGE_PASTE_CHAR_THRESHOLD + 1));
    let mut after = composer();
    after.restore_draft(before.take_draft());
    after.handle_key_event(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    assert_eq!(after.current_text(), "");
}

#[test]
fn new_session_draft_keeps_editor_configuration_and_original_state() {
    use nori_editor::EditorMode;
    let mut composer = composer();
    composer.set_kakoune_mode(true);
    type_chars_humanlike(&mut composer, &['a']);
    composer.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(composer.editor_mode(), EditorMode::Normal);
    let old_draft = composer.take_draft();
    assert_eq!(composer.textarea.keymap(), nori_editor::KeymapKind::Helix);
    assert_eq!(composer.editor_mode(), EditorMode::Insert);
    type_chars_humanlike(&mut composer, &['b']);
    assert_eq!(composer.current_text(), "b");
    composer.restore_draft(old_draft);
    assert_eq!(composer.current_text(), "a");
    assert_eq!(composer.editor_mode(), EditorMode::Normal);
}

#[test]
fn restored_insert_draft_does_not_paint_a_stale_selection() {
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    let mut composer = composer();
    composer.set_kakoune_mode(true);
    composer.insert_str("a restored draft");
    composer.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    composer.handle_key_event(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
    composer.handle_key_event(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE));
    composer.handle_key_event(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
    let parent = composer.take_draft();
    composer.insert_str("child draft");
    let _child = composer.take_draft();
    composer.restore_draft(parent);
    assert_eq!(composer.current_text(), "a restored draft");
    let area = Rect::new(0, 0, 30, 4);
    let mut buffer = Buffer::empty(area);
    composer.render(area, &mut buffer);
    let body = composer.rendered_body_area.get().expect("rendered body");
    for x in body.x..body.right() {
        let cell = &buffer[(x, body.y)];
        assert_eq!(cell.bg, Color::Reset);
        assert!(!cell.modifier.contains(Modifier::REVERSED));
    }
}
