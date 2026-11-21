//! The composer's text editor: [`nori_editor::Editor`] with what the composer adds to it (the
//! configurable editing hotkeys, shell highlighting, the composer's colours).
//!
//! Every text the user edits in the conversation is this editor: the draft, and every past
//! prompt walked back to with `↑` (it is loaded into the same editor).

use std::cell::Cell;
use std::ops::Range;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use nori_config::HotkeyAction;
use nori_config::HotkeyConfig;
use nori_editor::Change;
use nori_editor::CursorShape;
use nori_editor::Editor;
use nori_editor::EditorEvent;
use nori_editor::EditorMode;
use nori_editor::EditorStyle;
use nori_editor::Highlight;
use nori_editor::KeymapKind;
use nori_editor::chips::Chip;
use nori_editor::chips::ChipId;
use nori_editor::chips::ChipTone;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;

use crate::nori::hotkey_match::matches_binding;

/// The configurable editing hotkeys and the key each one stands for in the editor's keymaps.
const EDITING_HOTKEYS: [(HotkeyAction, KeyCode, KeyModifiers); 12] = [
    (
        HotkeyAction::MoveBackwardChar,
        KeyCode::Left,
        KeyModifiers::NONE,
    ),
    (
        HotkeyAction::MoveForwardChar,
        KeyCode::Right,
        KeyModifiers::NONE,
    ),
    (
        HotkeyAction::MoveBeginningOfLine,
        KeyCode::Home,
        KeyModifiers::NONE,
    ),
    (
        HotkeyAction::MoveEndOfLine,
        KeyCode::End,
        KeyModifiers::NONE,
    ),
    (
        HotkeyAction::MoveBackwardWord,
        KeyCode::Char('b'),
        KeyModifiers::ALT,
    ),
    (
        HotkeyAction::MoveForwardWord,
        KeyCode::Char('f'),
        KeyModifiers::ALT,
    ),
    (
        HotkeyAction::DeleteBackwardChar,
        KeyCode::Backspace,
        KeyModifiers::NONE,
    ),
    (
        HotkeyAction::DeleteForwardChar,
        KeyCode::Delete,
        KeyModifiers::NONE,
    ),
    (
        HotkeyAction::DeleteBackwardWord,
        KeyCode::Char('w'),
        KeyModifiers::CONTROL,
    ),
    (
        HotkeyAction::KillToEndOfLine,
        KeyCode::Char('k'),
        KeyModifiers::CONTROL,
    ),
    (
        HotkeyAction::KillToBeginningOfLine,
        KeyCode::Char('u'),
        KeyModifiers::CONTROL,
    ),
    (
        HotkeyAction::Yank,
        KeyCode::Char('y'),
        KeyModifiers::CONTROL,
    ),
];

#[derive(Debug)]
pub(crate) struct TextArea {
    editor: Editor,
    hotkey_config: HotkeyConfig,
    /// The text is a shell command: draw it highlighted. Set by the composer as it draws.
    shell_highlight: Cell<bool>,
    /// Where the text was last drawn, for clicks.
    area: Cell<Option<Rect>>,
}

impl TextArea {
    pub fn new() -> Self {
        Self {
            editor: Editor::new(KeymapKind::Standard),
            hotkey_config: HotkeyConfig::default(),
            shell_highlight: Cell::new(false),
            area: Cell::new(None),
        }
    }

    pub(crate) fn keymap(&self) -> KeymapKind {
        self.editor.keymap()
    }

    pub(crate) fn set_keymap(&mut self, keymap: KeymapKind) {
        self.editor.set_keymap(keymap);
    }

    /// Draw the text as a shell command from now on, or as plain text.
    pub(crate) fn set_shell_highlight(&self, on: bool) {
        self.shell_highlight.set(on);
    }

    pub fn set_hotkey_config(&mut self, config: HotkeyConfig) {
        self.hotkey_config = config;
    }

    pub fn hotkey_config(&self) -> &HotkeyConfig {
        &self.hotkey_config
    }

    /// The text, chips as their characters.
    pub fn text(&self) -> &str {
        self.editor.text()
    }

    pub fn is_empty(&self) -> bool {
        self.editor.is_empty()
    }

    /// Replace the text (a new undo history), the cursor at its end.
    pub fn set_text(&mut self, text: &str) {
        self.editor.set_text(text);
    }

    /// Insert `text` at every cursor (a paste).
    pub fn insert_str(&mut self, text: &str) {
        self.editor.insert_text(text);
    }

    /// Insert `text` at the primary cursor only.
    pub(crate) fn insert_at_cursor(&mut self, text: &str) {
        self.editor.insert_at_cursor(text);
    }

    pub fn replace_range(&mut self, range: Range<usize>, text: &str) {
        self.editor.replace_range(range, text);
    }

    pub fn cursor(&self) -> usize {
        self.editor.cursor()
    }

    pub fn set_cursor(&mut self, at: usize) {
        self.editor.set_cursor(at);
    }

    /// Insert an atomic chip labelled `label` at the cursor.
    pub(crate) fn insert_chip(&mut self, label: &str, tone: ChipTone) -> ChipId {
        self.editor.insert_chip(Chip {
            label: label.to_owned(),
            tone,
        })
    }

    pub(crate) fn chip_label(&self, id: ChipId) -> Option<&str> {
        self.editor.chip(id).map(|chip| chip.label.as_str())
    }

    /// A key for the editor. The configurable editing hotkeys are translated to the keys they
    /// stand for while typing (in Insert mode).
    pub fn input(&mut self, key: KeyEvent) -> Change {
        let key = if self.editor.mode() == EditorMode::Insert {
            EDITING_HOTKEYS
                .iter()
                .find(|(action, ..)| matches_binding(self.hotkey_config.binding_for(*action), &key))
                .map_or(key, |(_, code, modifiers)| KeyEvent {
                    code: *code,
                    modifiers: *modifiers,
                    ..key
                })
        } else {
            key
        };
        self.editor.handle_key(key)
    }

    pub(crate) fn mode(&self) -> EditorMode {
        self.editor.mode()
    }

    /// Whether the editor is modal and in Normal mode.
    pub fn is_normal_mode(&self) -> bool {
        self.editor.keymap().is_modal() && self.editor.mode() == EditorMode::Normal
    }

    /// Whether a key menu, count or register waits for its next key.
    pub(crate) fn is_pending(&self) -> bool {
        self.editor.pending().is_some() || matches!(self.editor.mode(), EditorMode::Menu(_))
    }

    /// Whether `Esc` means something to the editor now (see [`Editor::wants_escape`]).
    pub(crate) fn wants_escape(&self) -> bool {
        self.editor.wants_escape()
    }

    pub(crate) fn cursor_shape(&self) -> CursorShape {
        self.editor.cursor_shape()
    }

    /// Back to Insert mode with nothing pending, as after a prompt is sent.
    pub(crate) fn enter_insert(&mut self) {
        self.editor.enter_insert();
    }

    pub(crate) fn take_events(&mut self) -> Vec<EditorEvent> {
        self.editor.take_events()
    }

    pub(crate) fn editor_mut(&mut self) -> &mut Editor {
        &mut self.editor
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        self.editor.desired_height(width)
    }

    /// The wrapped row the cursor is on at `width`, and how many rows the text wraps to.
    pub(crate) fn cursor_visual_row(&self, width: u16) -> (usize, usize) {
        self.editor.cursor_row(width)
    }

    /// Where the terminal cursor goes when the text is drawn in `area`.
    pub fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.editor
            .cursor_position(area)
            .map(|position| (position.x, position.y))
    }

    /// Put the cursor where a click at `(column, row)` landed. Returns whether it was on the text.
    pub(crate) fn place_cursor_at(&mut self, column: u16, row: u16) -> bool {
        let Some(area) = self.area.get() else {
            return false;
        };
        self.editor.click(area, column, row)
    }

    pub(crate) fn render(&self, area: Rect, buf: &mut Buffer) {
        self.area.set(Some(area));
        let theme = crate::style::component_theme();
        let style = EditorStyle {
            text: Style::default(),
            // A background every palette shows: the cyan accent behind the text.
            selection: Style::default()
                .bg(theme.accent_cyan.fg.unwrap_or(ratatui::style::Color::Cyan))
                .fg(ratatui::style::Color::Black),
            cursor: Style::default().add_modifier(Modifier::REVERSED),
            attachment: theme.accent_amber.add_modifier(Modifier::BOLD),
            reference: theme.accent_cyan.add_modifier(Modifier::UNDERLINED),
            directive: theme.accent_amber.add_modifier(Modifier::ITALIC),
            prompt_label: Style::default().add_modifier(Modifier::DIM),
        };
        let highlights = if self.shell_highlight.get() {
            shell_highlights(self.editor.text())
        } else {
            Vec::new()
        };
        self.editor.render(area, buf, &style, &highlights);
    }
}

/// Shell syntax colours for `text`, as stretches of it.
fn shell_highlights(text: &str) -> Vec<Highlight> {
    let mut highlights = Vec::new();
    let mut offset = 0;
    for (line, spans) in text
        .split('\n')
        .zip(crate::render::bash_highlight::highlight_shell_lines(
            text,
            &crate::render::bash_highlight::ShellStyles::live(),
        ))
    {
        let mut at = offset;
        for span in spans {
            let end = (at + span.content.len()).min(offset + line.len());
            if at < end {
                highlights.push(Highlight {
                    range: at..end,
                    style: span.style,
                });
            }
            at = end;
        }
        offset += line.len() + 1;
    }
    highlights
}
