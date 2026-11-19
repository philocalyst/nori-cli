//! Kakoune editing adapter backed by modalkit's real textbox implementation.

use std::cell::Cell;
use std::cell::RefCell;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use modalkit::actions::Action;
use modalkit::actions::Scrollable;
use modalkit::editing::application::EmptyInfo;
use modalkit::editing::key::KeyManager;
use modalkit::editing::store::Store;
use modalkit::env::kak::KakouneMode;
use modalkit::env::kak::keybindings::default_kakoune_keys;
use modalkit::key::TerminalKey;
use modalkit::keybindings::BindingMachine;
use modalkit_ratatui::TerminalCursor;
use modalkit_ratatui::textbox::TextBox;
use modalkit_ratatui::textbox::TextBoxState;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::StatefulWidget;

pub(super) struct KakouneEditor {
    bindings: KeyManager<TerminalKey, Action<EmptyInfo>, modalkit::prelude::RepeatType>,
    textbox: RefCell<TextBoxState>,
    store: Store<EmptyInfo>,
    cursor_needs_sync: Cell<bool>,
    selection_mode: bool,
    previous_width: Cell<u16>,
}

impl std::fmt::Debug for KakouneEditor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KakouneEditor").finish_non_exhaustive()
    }
}

impl KakouneEditor {
    pub(super) fn new() -> Self {
        let mut store = Store::default();
        let textbox = TextBoxState::new(store.load_buffer(String::from("*nori-prompt*")));
        let mut editor = Self {
            bindings: KeyManager::new(default_kakoune_keys()),
            textbox: RefCell::new(textbox),
            store,
            cursor_needs_sync: Cell::new(false),
            selection_mode: false,
            previous_width: Cell::new(0),
        };
        // The prompt is ready for typing on first launch.
        editor.input(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE));
        editor
    }

    pub(super) fn mode(&self) -> KakouneMode {
        match self
            .bindings
            .show_mode()
            .as_deref()
            .and_then(|mode| mode.split_whitespace().next())
        {
            Some("insert") => KakouneMode::Insert,
            Some("prompt") => KakouneMode::Prompt,
            Some("goto") => KakouneMode::Goto,
            Some("view") => KakouneMode::View,
            Some("user") => KakouneMode::User,
            Some("object") => KakouneMode::ObjectSelect,
            _ => KakouneMode::Normal,
        }
    }

    pub(super) fn set_text(&self, text: &str) {
        if self.text() != text {
            self.textbox.borrow_mut().set_text(format!("{text}\n"));
            self.cursor_needs_sync.set(true);
        }
    }

    pub(super) fn text(&self) -> String {
        let mut text = self.textbox.borrow().get_text();
        text.pop(); // modalkit buffers always end with a sentinel newline.
        text
    }

    pub(super) fn cursor(&self, text: &str) -> usize {
        let cursor = self.textbox.borrow_mut().get_cursor();
        let mut lines = text.split_inclusive('\n');
        let prefix: usize = lines.by_ref().take(cursor.y).map(str::len).sum();
        let line = lines.next().unwrap_or("");
        prefix
            + line
                .char_indices()
                .nth(cursor.x)
                .map_or(line.len(), |(i, _)| i)
    }

    pub(super) fn cursor_needs_sync(&self) -> bool {
        self.cursor_needs_sync.get()
    }

    pub(super) fn set_cursor(&mut self, pos: usize) {
        use modalkit::actions::EditAction;
        use modalkit::actions::Editable;
        use modalkit::actions::EditorAction;
        use modalkit::editing::context::EditContext;
        use modalkit::prelude::Count;
        use modalkit::prelude::EditTarget;
        use modalkit::prelude::MoveType;

        let action = EditorAction::Edit(
            EditAction::Motion.into(),
            EditTarget::Motion(MoveType::BufferByteOffset, Count::Exact(pos + 1)),
        );
        let _ = self.textbox.borrow_mut().editor_command(
            &action,
            &EditContext::default(),
            &mut self.store,
        );
        self.cursor_needs_sync.set(false);
    }

    pub(super) fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut textbox = self.textbox.borrow_mut();
        let previous_width = self.previous_width.replace(area.width);
        if previous_width > 0 && area.width > previous_width && area.height > 0 {
            // Modalkit's wrapped viewport retains a horizontal corner after
            // scrolling an exact-width caret onto the next row. Reposition
            // the viewport without changing the buffer, cursor, or selection.
            textbox.set_term_info(area);
            textbox.set_wrap(false);
            let _ = textbox.scroll(
                &modalkit::prelude::ScrollStyle::CursorPos(
                    modalkit::prelude::MovePosition::End,
                    modalkit::prelude::Axis::Horizontal,
                ),
                &modalkit::editing::context::EditContext::default(),
                &mut Store::<EmptyInfo>::default(),
            );
            textbox.set_wrap(true);
        }
        TextBox::new().render(area, buf, &mut *textbox);
        // Modalkit keeps its selection when changing modes. Insert mode uses
        // a caret, so a retained selection must not paint the restored draft.
        if self.mode() == KakouneMode::Insert {
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    buf[(x, y)]
                        .modifier
                        .remove(ratatui::style::Modifier::REVERSED);
                }
            }
        }
    }

    pub(super) fn desired_height(&self, width: u16) -> u16 {
        if width == 0 {
            return 1;
        }
        let mut textbox = self.textbox.borrow_mut();
        textbox.set_term_info(Rect::new(0, 0, width, 1));
        u16::try_from(textbox.has_lines(usize::from(u16::MAX))).unwrap_or(u16::MAX)
    }

    pub(super) fn term_cursor(&self) -> Option<(u16, u16)> {
        self.textbox.borrow().get_term_cursor()
    }

    pub(super) fn input(&mut self, event: KeyEvent) {
        if event.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        let mut event = event;
        if event.modifiers == KeyModifiers::NONE && self.mode() == KakouneMode::Normal {
            if event.code == KeyCode::Char('v') {
                self.selection_mode = !self.selection_mode;
                return;
            }
            if self.selection_mode {
                if let KeyCode::Char(ch @ ('h' | 'j' | 'k' | 'l' | 'w' | 'b' | 'e')) = event.code {
                    event.code = KeyCode::Char(ch.to_ascii_uppercase());
                    event.modifiers = KeyModifiers::SHIFT;
                } else {
                    self.selection_mode = false;
                }
            }
        }
        self.bindings.input_key(event.into());
        while let Some((action, context)) = self.bindings.pop() {
            match action {
                Action::Editor(action) => {
                    let _ = modalkit::actions::Editable::editor_command(
                        &mut *self.textbox.borrow_mut(),
                        &action,
                        &context,
                        &mut self.store,
                    );
                }
                Action::Repeat(repeat) => self.bindings.repeat(repeat, Some(context)),
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KakouneEditor;
    use pretty_assertions::assert_eq;

    #[test]
    fn kakoune_preserves_user_newlines() {
        let editor = KakouneEditor::new();
        editor.set_text("first\n\n");
        assert_eq!(editor.text(), "first\n\n");
    }

    #[test]
    fn exact_width_draft_stays_visible_when_shrunk_and_expanded() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let mut editor = KakouneEditor::new();
        let draft = "draft survives child switch";
        for ch in draft.chars() {
            editor.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        editor.input(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        let wide_h = editor.desired_height(119);
        let mut wide_buf = Buffer::empty(Rect::new(0, 0, 119, wide_h));
        editor.render(Rect::new(0, 0, 119, wide_h), &mut wide_buf);

        for narrow_w in [27_u16, 28, 26] {
            let narrow_h = editor.desired_height(narrow_w);
            let mut narrow_buf = Buffer::empty(Rect::new(0, 0, narrow_w, narrow_h));
            editor.render(Rect::new(0, 0, narrow_w, narrow_h), &mut narrow_buf);
            let text: String = (0..narrow_h)
                .map(|y| {
                    (0..narrow_w)
                        .map(|x| narrow_buf[(x, y)].symbol())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("");
            assert!(
                text.contains("draft survives") && text.contains("child switch"),
                "width={narrow_w} height={narrow_h} rendered={text:?}"
            );
        }

        let rewide_h = editor.desired_height(119);
        assert_eq!(rewide_h, 1);
        let mut rewide_buf = Buffer::empty(Rect::new(0, 0, 119, rewide_h));
        editor.render(Rect::new(0, 0, 119, rewide_h), &mut rewide_buf);
        let draft_w = u16::try_from(draft.len()).expect("width");
        let rewide_row0: String = (0..draft_w).map(|x| rewide_buf[(x, 0)].symbol()).collect();
        assert_eq!(rewide_row0, draft);
    }
}
