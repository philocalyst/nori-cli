//! The editor driven by keys, as a user drives it.

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;

use crate::Editor;
use crate::EditorMode;
use crate::KeymapKind;
use crate::chips::Chip;
use crate::chips::ChipTone;

/// Press the keys written in `keys`: characters as themselves, `<Esc>`, `<Enter>`, `<BS>`,
/// `<Left>`, `<Up>`, `<C-x>`, `<A-x>`, `<S-Left>` and the like for the rest.
fn press(editor: &mut Editor, keys: &str) {
    let mut rest = keys;
    while let Some(ch) = rest.chars().next() {
        if ch == '<'
            && let Some(end) = rest.find('>')
            && end > 1
        {
            editor.handle_key(named(&rest[1..end]));
            rest = &rest[end + 1..];
            continue;
        }
        let modifiers = if ch.is_ascii_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        editor.handle_key(KeyEvent::new(KeyCode::Char(ch), modifiers));
        rest = &rest[ch.len_utf8()..];
    }
}

fn named(name: &str) -> KeyEvent {
    let mut modifiers = KeyModifiers::NONE;
    let mut key = name;
    while let Some((prefix, rest)) = key.split_once('-')
        && !rest.is_empty()
    {
        modifiers |= match prefix {
            "C" => KeyModifiers::CONTROL,
            "A" => KeyModifiers::ALT,
            "S" => KeyModifiers::SHIFT,
            other => panic!("unknown modifier {other}"),
        };
        key = rest;
    }
    let code = match key {
        "Esc" => KeyCode::Esc,
        "Enter" => KeyCode::Enter,
        "BS" => KeyCode::Backspace,
        "Del" => KeyCode::Delete,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "Space" => KeyCode::Char(' '),
        single if single.chars().count() == 1 => {
            KeyCode::Char(single.chars().next().unwrap_or(' '))
        }
        other => panic!("unknown key {other}"),
    };
    KeyEvent::new(code, modifiers)
}

fn helix(text: &str) -> Editor {
    let mut editor = Editor::new(KeymapKind::Helix);
    press(&mut editor, text);
    editor
}

#[test]
fn helix_starts_ready_to_type() {
    let editor = helix("hello");
    assert_eq!(editor.text(), "hello");
    assert_eq!(editor.mode(), EditorMode::Insert);
    assert_eq!(editor.cursor(), 5);
}

#[test]
fn helix_word_motions_select_and_d_deletes() {
    let mut editor = helix("hello world<Esc>");
    assert_eq!(editor.mode(), EditorMode::Normal);
    press(&mut editor, "b");
    assert_eq!(editor.selected_text(), vec!["world".to_owned()]);
    press(&mut editor, "d");
    assert_eq!(editor.text(), "hello ");
}

#[test]
fn helix_a_appends_after_the_selection_and_i_inserts_before_it() {
    // On one character: `a` types after it, `i` before it.
    let mut editor = helix("abc<Esc>gh");
    press(&mut editor, "aX<Esc>");
    assert_eq!(editor.text(), "aXbc");
    let mut editor = helix("abc<Esc>gh");
    press(&mut editor, "iX<Esc>");
    assert_eq!(editor.text(), "Xabc");
    // On a selected word: after its last character, before its first.
    let mut editor = helix("one two<Esc>gh");
    press(&mut editor, "eaX<Esc>");
    assert_eq!(editor.text(), "oneX two");
    // At the end of the line.
    let mut editor = helix("ab<Esc>gl");
    press(&mut editor, "aX<Esc>");
    assert_eq!(editor.text(), "abX");
}

#[test]
fn helix_x_selects_the_line_and_c_changes_it() {
    let mut editor = helix("first line<Esc>");
    press(&mut editor, "xc");
    assert_eq!(editor.mode(), EditorMode::Insert);
    press(&mut editor, "second");
    assert_eq!(editor.text(), "second");
}

#[test]
fn helix_copies_selections_down_and_types_at_each() {
    let mut editor = helix("a<Enter>b<Enter>c<Esc>gg");
    press(&mut editor, "CC");
    assert_eq!(editor.selections().len(), 3);
    press(&mut editor, "ix");
    assert_eq!(editor.text(), "xa\nxb\nxc");
}

#[test]
fn helix_surround_add_and_delete() {
    let mut editor = helix("hello<Esc>");
    press(&mut editor, "%ms(");
    assert_eq!(editor.text(), "(hello)");
    press(&mut editor, "md(");
    assert_eq!(editor.text(), "hello");
}

#[test]
fn helix_select_inside_brackets() {
    let mut editor = helix("call(arg, two)<Esc>");
    editor.set_cursor(6);
    press(&mut editor, "mi(d");
    assert_eq!(editor.text(), "call()");
}

#[test]
fn helix_undo_and_redo_whole_insert_sessions() {
    let mut editor = helix("one<Esc>a two<Esc>");
    assert_eq!(editor.text(), "one two");
    press(&mut editor, "u");
    assert_eq!(editor.text(), "one");
    press(&mut editor, "U");
    assert_eq!(editor.text(), "one two");
}

#[test]
fn helix_search_selects_the_match() {
    let mut editor = helix("one two one three<Esc>gg");
    press(&mut editor, "/three<Enter>");
    assert_eq!(editor.mode(), EditorMode::Normal);
    assert_eq!(editor.selected_text(), vec!["three".to_owned()]);
}

#[test]
fn helix_select_mode_extends() {
    let mut editor = helix("alpha beta gamma<Esc>gg");
    press(&mut editor, "vww");
    assert_eq!(editor.mode(), EditorMode::Select);
    assert_eq!(editor.selected_text(), vec!["alpha beta ".to_owned()]);
}

#[test]
fn standard_types_selects_and_undoes_by_word() {
    let mut editor = Editor::new(KeymapKind::Standard);
    press(&mut editor, "one two three");
    assert_eq!(editor.mode(), EditorMode::Insert);
    press(&mut editor, "<C-z>");
    assert_eq!(editor.text(), "one two ");
    press(&mut editor, "<S-Left><S-Left>");
    assert_eq!(editor.selected_text(), vec!["o ".to_owned()]);
    // Esc is the host's in a modeless editor.
    assert!(!editor.wants_escape());
}

#[test]
fn readline_keys_work_in_insert_mode() {
    let mut editor = helix("hello world");
    press(&mut editor, "<C-w>");
    assert_eq!(editor.text(), "hello ");
    press(&mut editor, "<C-a>X");
    assert_eq!(editor.text(), "Xhello ");
}

#[test]
fn vim_starts_in_insert_and_dd_deletes_a_line() {
    let mut editor = Editor::new(KeymapKind::Vim);
    press(&mut editor, "one<Enter>two<Esc>");
    assert_eq!(editor.mode(), EditorMode::Normal);
    press(&mut editor, "dd");
    assert_eq!(editor.text(), "one");
}

#[test]
fn vertical_moves_follow_wrapped_rows() {
    let mut editor = helix("aaaa bbbb cccc");
    // Five text columns a row: "aaaa ", "bbbb ", "cccc".
    let _ = editor.desired_height(6);
    assert_eq!(editor.cursor_row(6), (2, 3));
    press(&mut editor, "<Up>");
    assert_eq!(editor.cursor(), 9);
    press(&mut editor, "<Up>");
    assert_eq!(editor.cursor(), 4);
    assert!(!editor.move_vertically(false));
}

#[test]
fn a_chip_is_deleted_as_one() {
    let mut editor = helix("see ");
    let id = editor.insert_chip(Chip {
        label: "[Pasted 2000 chars]".into(),
        tone: ChipTone::Attachment,
    });
    assert_eq!(editor.text(), format!("see {}", id.as_char()));
    press(&mut editor, "<BS>");
    assert_eq!(editor.text(), "see ");
}

#[test]
fn set_text_puts_the_cursor_at_the_end() {
    let mut editor = helix("");
    editor.set_text("replaced\ntext");
    assert_eq!(editor.cursor(), "replaced\ntext".len());
    editor.replace_range(0..8, "new");
    assert_eq!(editor.text(), "new\ntext");
    assert_eq!(editor.cursor(), 3);
}

#[test]
fn readline_kill_and_yank() {
    let mut editor = helix("hello world");
    press(&mut editor, "<C-a><C-k>");
    assert_eq!(editor.text(), "");
    press(&mut editor, "<C-y>");
    assert_eq!(editor.text(), "hello world");
}

#[test]
fn x_after_undo_selects_the_whole_line() {
    let mut editor = helix("hello world from the helix editor<Esc>");
    press(&mut editor, "bbdu");
    assert_eq!(editor.text(), "hello world from the helix editor");
    press(&mut editor, "x");
    assert_eq!(
        editor.selected_text(),
        vec!["hello world from the helix editor".to_owned()]
    );
    press(&mut editor, "cnew");
    assert_eq!(editor.text(), "new");
}

#[test]
fn x_again_takes_the_next_line() {
    let mut editor = helix("one<Enter>two<Enter>three<Esc>gg");
    press(&mut editor, "x");
    assert_eq!(editor.selected_text(), vec!["one\n".to_owned()]);
    press(&mut editor, "x");
    assert_eq!(editor.selected_text(), vec!["one\ntwo\n".to_owned()]);
    press(&mut editor, "d");
    assert_eq!(editor.text(), "three");
}

#[test]
fn a_fresh_editor_answers_cursor_queries() {
    let editor = Editor::new(KeymapKind::Helix);
    assert_eq!(editor.cursor(), 0);
    assert_eq!(editor.cursor_row(40), (0, 1));
    let fresh = Editor::new(KeymapKind::Standard);
    assert_eq!(fresh.selections().len(), 1);
}

/// Many keys in every keymap: the editor never panics, and its cursors and layout stay valid.
#[test]
fn random_keys_never_break_the_editor() {
    const KEYS: &[&str] = &[
        "a",
        "b",
        "w",
        "e",
        "W",
        "B",
        "E",
        "x",
        "X",
        "d",
        "c",
        "y",
        "p",
        "P",
        "u",
        "U",
        "i",
        "A",
        "o",
        "O",
        "v",
        "%",
        ";",
        ",",
        "C",
        "<A-C>",
        "<A-s>",
        "J",
        "~",
        ">",
        "<",
        "mi(",
        "ma\"",
        "ms(",
        "md(",
        "mm",
        "gg",
        "ge",
        "gh",
        "gl",
        "h",
        "j",
        "k",
        "l",
        "f(",
        "t ",
        "r!",
        "<Esc>",
        "<Enter>",
        "<BS>",
        "<Del>",
        "<Up>",
        "<Down>",
        "<Left>",
        "<Right>",
        "<Home>",
        "<End>",
        "<C-w>",
        "<C-k>",
        "<C-u>",
        "<C-y>",
        "<C-a>",
        "<C-e>",
        "<S-Left>",
        "<C-z>",
        "(",
        ")",
        "é",
        "漢",
        "👩\u{200D}💻",
        " ",
        "\t",
        "2",
        "\"a",
        ".",
        "/a<Enter>",
        "n",
        "N",
        "*",
    ];
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    for keymap in [KeymapKind::Helix, KeymapKind::Vim, KeymapKind::Standard] {
        let mut editor = Editor::new(keymap);
        press(
            &mut editor,
            "hello (world) \"quoted\" text<Enter>second line here",
        );
        for _ in 0..3000 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let key = KEYS[(seed % KEYS.len() as u64) as usize];
            press(&mut editor, key);
            let text = editor.text();
            for selection in editor.selections() {
                assert!(selection.head <= text.len(), "{keymap:?} after {key:?}");
                assert!(
                    text.is_char_boundary(selection.head),
                    "{keymap:?} after {key:?}"
                );
                assert!(
                    text.is_char_boundary(selection.anchor),
                    "{keymap:?} after {key:?}"
                );
            }
            let width = 3 + (seed % 40) as u16;
            let (row, rows) = editor.cursor_row(width);
            assert!(row < rows, "{keymap:?} after {key:?}");
        }
    }
}

#[test]
fn helix_counts_registers_and_replace() {
    let mut editor = helix("one two three four<Esc>gg");
    press(&mut editor, "2w");
    assert_eq!(editor.selected_text(), vec!["two ".to_owned()]);
    press(&mut editor, "\"ay");
    press(&mut editor, "gl\"ap");
    assert_eq!(editor.text(), "one two three fourtwo ");
    press(&mut editor, "ggwr-");
    assert_eq!(editor.text(), "----two three fourtwo ");
}

#[test]
fn helix_select_regex_and_multi_cursor_change() {
    let mut editor = helix("foo bar foo baz foo<Esc>");
    press(&mut editor, "%sfoo<Enter>");
    assert_eq!(editor.selections().len(), 3);
    press(&mut editor, "cqux<Esc>");
    assert_eq!(editor.text(), "qux bar qux baz qux");
}

#[test]
fn helix_surround_replace_and_case() {
    let mut editor = helix("say (hello)<Esc>");
    editor.set_cursor(6);
    press(&mut editor, "mr([");
    assert_eq!(editor.text(), "say [hello]");
    press(&mut editor, "%~");
    assert_eq!(editor.text(), "SAY [HELLO]");
}

#[test]
fn helix_search_selection_then_next() {
    let mut editor = helix("ab cd ab ef ab<Esc>gg");
    press(&mut editor, "e*");
    press(&mut editor, "n");
    assert_eq!(editor.primary().head, 7);
    assert_eq!(editor.selected_text()[0], "ab");
}

#[test]
fn helix_macro_records_and_replays() {
    let mut editor = helix("a<Enter>b<Enter>c<Esc>gg");
    press(&mut editor, "QI- <Esc>jQ");
    press(&mut editor, "q");
    assert_eq!(editor.text(), "- a\n- b\nc");
}

#[test]
fn helix_indent_and_join() {
    let mut editor = helix("one<Enter>two<Esc>gg");
    press(&mut editor, ">");
    assert_eq!(editor.text(), "    one\ntwo");
    press(&mut editor, "%>");
    assert_eq!(editor.text(), "        one\n    two");
    press(&mut editor, "<");
    assert_eq!(editor.text(), "    one\ntwo");
    press(&mut editor, "%J");
    assert_eq!(editor.text(), "    one two");
}

#[test]
fn a_pending_operator_or_count_keeps_escape_for_the_editor() {
    let mut vim = Editor::new(KeymapKind::Vim);
    press(&mut vim, "one<Esc>");
    assert!(
        !vim.wants_escape(),
        "Normal mode at rest leaves Esc to the host"
    );
    press(&mut vim, "d");
    assert!(vim.wants_escape(), "Vim `d` waits for a motion");
    press(&mut vim, "<Esc>");
    assert!(!vim.wants_escape());
    assert_eq!(vim.text(), "one");
    press(&mut vim, "2");
    assert!(vim.wants_escape(), "a count waits for its command");

    let mut hx = helix("one<Esc>");
    press(&mut hx, "2");
    assert!(hx.wants_escape(), "a Helix count waits for its command");
}

#[test]
fn helix_space_menu_asks_the_host() {
    let mut editor = helix("draft<Esc>");
    press(&mut editor, "<Space>?");
    assert_eq!(
        editor.take_events(),
        vec![crate::EditorEvent::Host("palette".to_owned())]
    );
    press(&mut editor, "<Space>f");
    assert_eq!(
        editor.take_events(),
        vec![crate::EditorEvent::Host("files".to_owned())]
    );
    assert_eq!(editor.mode(), EditorMode::Normal);
}

#[test]
fn a_chip_goes_in_once_whatever_the_cursors() {
    let mut editor = helix("a<Enter>b<Esc>ggC");
    assert_eq!(editor.selections().len(), 2);
    let id = editor.insert_chip(Chip {
        label: "x".into(),
        tone: ChipTone::Directive,
    });
    assert_eq!(editor.text().matches(id.as_char()).count(), 1);
}
